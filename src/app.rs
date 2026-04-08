use crate::cli::{Cli, Command, RunCommand, ServerCommand, WorkspaceCommand};
use crate::client::{
    normalize_endpoint, read_delegated_spawn_request, write_delegated_spawn_response,
    ClientRuntime, ClientRuntimeConfig, ClientRuntimeError, DelegatedSpawnRequest,
};
use crate::config::AppConfig;
use crate::console::ConsoleState;
use crate::pty::{ExitStatus, PtyHandle, PtyManager, PtySize, SpawnRequest, PTY_EOF_ERRNO};
use crate::renderer::{RenderContext, RenderError, RenderFrame, Renderer, RendererState};
use crate::scheduler::{SchedulerState, SchedulingAction};
use crate::server::{ServerRuntime, ServerRuntimeConfig, ServerRuntimeError};
use crate::session::{SessionAddress, SessionRegistry, SessionStatus};
use crate::terminal::{TerminalEngine, TerminalRuntime};
use crate::transport::{read_transport_envelope, write_transport_envelope};
use std::collections::HashMap;
use std::env;
use std::error::Error;
use std::fmt;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const EVENT_LOOP_TICK: Duration = Duration::from_millis(50);
const PICKER_ESCAPE_TIMEOUT_MS: u128 = 150;
const CLEAR_FRAME: &str = "\x1b[H\x1b[2J";
const RESTORE_SCREEN: &str = "\x1b[2J\x1b[H\x1b[?25h";
const SHORTCUT_INTERRUPT_EXIT: u8 = 0x03;
const ANSI_RESET: &str = "\x1b[0m";
const ANSI_FG_ACCENT: &str = "\x1b[38;5;81m";
const ANSI_FG_NOTICE: &str = "\x1b[38;5;120m";
const ANSI_BG_BAR: &str = "\x1b[48;5;24m\x1b[38;5;255m";
const ANSI_BG_KEYS: &str = "\x1b[48;5;236m\x1b[38;5;252m";
const ANSI_BG_COMMAND: &str = "\x1b[48;5;238m\x1b[38;5;255m";
const ANSI_BG_PICKER: &str = "\x1b[48;5;235m\x1b[38;5;250m";
const ANSI_BG_PICKER_ACTIVE: &str = "\x1b[48;5;31m\x1b[38;5;255m";

pub fn run() -> Result<(), AppError> {
    let cli = Cli::parse(std::env::args_os())?;
    let config = AppConfig::from_env();
    let mut app = App::new(config);

    app.execute(cli.command)
}

struct App {
    config: AppConfig,
    sessions: SessionRegistry,
    pty: PtyManager,
    terminal: TerminalRuntime,
}

impl App {
    fn new(config: AppConfig) -> Self {
        Self {
            config,
            sessions: SessionRegistry::new(),
            pty: PtyManager::new(),
            terminal: TerminalRuntime::stdio(),
        }
    }

    fn execute(&mut self, command: Command) -> Result<(), AppError> {
        match command {
            Command::Workspace(workspace) => self.handle_workspace(workspace),
            Command::Run(run) => self.handle_run(run),
            Command::Server(server) => self.handle_server(server),
            Command::Help(help) => {
                print_banner();
                println!("{help}");
                Ok(())
            }
        }
    }

    fn handle_workspace(&mut self, command: WorkspaceCommand) -> Result<(), AppError> {
        let runtime = self
            .config
            .runtime_for_workspace(command.node_id.as_deref(), command.connect.as_deref());
        self.run_local_workspace(&runtime)
    }

    fn run_local_workspace(&mut self, runtime: &AppConfig) -> Result<(), AppError> {
        let terminal_snapshot = self.terminal.snapshot()?;
        if !terminal_snapshot.input_is_tty || !terminal_snapshot.output_is_tty {
            return Err(AppError::Terminal(crate::terminal::TerminalError::NotTty(
                "workspace console".to_string(),
            )));
        }

        let _alternate_screen = self.terminal.enter_alternate_screen()?;
        let _raw_mode = self.terminal.enter_raw_mode()?;
        let mut console = ConsoleState::new("workspace-console");
        let mut scheduler = SchedulerState::new();
        let renderer = Renderer::new();
        let mut renderer_state = RendererState::default();
        let mut input_tracker = InputTracker::default();
        let mut command_prompt = CommandPromptState::default();
        let mut hosted = HashMap::<SessionAddress, HostedSession>::new();

        let (tx, rx) = mpsc::channel();
        spawn_stdin_reader(tx.clone());

        let initial_session = self.spawn_default_shell_session(
            &runtime.node.node_id,
            terminal_snapshot.size,
            &mut hosted,
            &tx,
        )?;
        console.focus(initial_session);

        self.render_workspace_console(
            &mut renderer_state,
            &renderer,
            &console,
            &scheduler,
            &command_prompt,
        )?;
        let mut last_waiting_count = scheduler.waiting_queue().entries().len();
        let mut should_exit = false;

        while !should_exit {
            match rx.recv_timeout(EVENT_LOOP_TICK) {
                Ok(RuntimeEvent::Input(bytes)) => {
                    let input_received_at = now_unix_ms();
                    if let Some(outcome) = command_prompt.handle_picker_navigation(
                        &bytes,
                        &self.sessions.list(),
                        console.focused_session.as_ref(),
                        input_received_at,
                    ) {
                        if matches!(outcome, PickerNavigationOutcome::Render) {
                            self.render_workspace_console(
                                &mut renderer_state,
                                &renderer,
                                &console,
                                &scheduler,
                                &command_prompt,
                            )?;
                        }
                    } else if matches!(parse_console_action(&bytes), Some(ConsoleAction::QuitHost))
                    {
                        should_exit = true;
                    } else if let Some(outcome) = command_prompt.handle_input(&bytes) {
                        should_exit = self.apply_command_outcome(
                            outcome,
                            runtime,
                            terminal_snapshot.size,
                            &mut hosted,
                            &tx,
                            &mut console,
                            &mut scheduler,
                            &mut renderer_state,
                            &renderer,
                            &mut command_prompt,
                            RenderSurface::Workspace,
                        )?;
                    } else if let Some(action) = parse_console_action(&bytes) {
                        match action {
                            ConsoleAction::PreviousSession
                                if command_prompt.move_picker_previous(
                                    &self.sessions.list(),
                                    console.focused_session.as_ref(),
                                ) =>
                            {
                                self.render_workspace_console(
                                    &mut renderer_state,
                                    &renderer,
                                    &console,
                                    &scheduler,
                                    &command_prompt,
                                )?;
                            }
                            ConsoleAction::NextSession
                                if command_prompt.move_picker_next(
                                    &self.sessions.list(),
                                    console.focused_session.as_ref(),
                                ) =>
                            {
                                self.render_workspace_console(
                                    &mut renderer_state,
                                    &renderer,
                                    &console,
                                    &scheduler,
                                    &command_prompt,
                                )?;
                            }
                            _ => {
                                should_exit = self.apply_workspace_action(
                                    action,
                                    runtime,
                                    terminal_snapshot.size,
                                    &mut hosted,
                                    &tx,
                                    &mut console,
                                    &mut scheduler,
                                    &mut renderer_state,
                                    &renderer,
                                    &mut command_prompt,
                                )?;
                            }
                        }
                    } else if let Some(index) =
                        command_prompt.pick_session_index(&bytes, &self.sessions.list())
                    {
                        should_exit = self.apply_workspace_action(
                            ConsoleAction::FocusIndex(index),
                            runtime,
                            terminal_snapshot.size,
                            &mut hosted,
                            &tx,
                            &mut console,
                            &mut scheduler,
                            &mut renderer_state,
                            &renderer,
                            &mut command_prompt,
                        )?;
                    } else if command_prompt.submit_overlay(&bytes) {
                        if let Some(index) = command_prompt.selected_picker_index(
                            &self.sessions.list(),
                            console.focused_session.as_ref(),
                        ) {
                            should_exit = self.apply_workspace_action(
                                ConsoleAction::FocusIndex(index),
                                runtime,
                                terminal_snapshot.size,
                                &mut hosted,
                                &tx,
                                &mut console,
                                &mut scheduler,
                                &mut renderer_state,
                                &renderer,
                                &mut command_prompt,
                            )?;
                        } else {
                            should_exit = self.apply_workspace_action(
                                ConsoleAction::DismissOverlay,
                                runtime,
                                terminal_snapshot.size,
                                &mut hosted,
                                &tx,
                                &mut console,
                                &mut scheduler,
                                &mut renderer_state,
                                &renderer,
                                &mut command_prompt,
                            )?;
                        }
                    } else {
                        let mut residual = Vec::new();
                        let mut handled_control = false;

                        for &byte in &bytes {
                            let single = [byte];
                            if let Some(outcome) = command_prompt.handle_picker_navigation(
                                &single,
                                &self.sessions.list(),
                                console.focused_session.as_ref(),
                                now_unix_ms(),
                            ) {
                                handled_control = true;
                                if matches!(outcome, PickerNavigationOutcome::Render) {
                                    self.render_workspace_console(
                                        &mut renderer_state,
                                        &renderer,
                                        &console,
                                        &scheduler,
                                        &command_prompt,
                                    )?;
                                }
                            } else if let Some(outcome) = command_prompt.handle_input(&single) {
                                handled_control = true;
                                should_exit = self.apply_command_outcome(
                                    outcome,
                                    runtime,
                                    terminal_snapshot.size,
                                    &mut hosted,
                                    &tx,
                                    &mut console,
                                    &mut scheduler,
                                    &mut renderer_state,
                                    &renderer,
                                    &mut command_prompt,
                                    RenderSurface::Workspace,
                                )?;
                            } else if let Some(action) = parse_console_action(&single) {
                                handled_control = true;
                                match action {
                                    ConsoleAction::PreviousSession
                                        if command_prompt.move_picker_previous(
                                            &self.sessions.list(),
                                            console.focused_session.as_ref(),
                                        ) =>
                                    {
                                        self.render_workspace_console(
                                            &mut renderer_state,
                                            &renderer,
                                            &console,
                                            &scheduler,
                                            &command_prompt,
                                        )?;
                                    }
                                    ConsoleAction::NextSession
                                        if command_prompt.move_picker_next(
                                            &self.sessions.list(),
                                            console.focused_session.as_ref(),
                                        ) =>
                                    {
                                        self.render_workspace_console(
                                            &mut renderer_state,
                                            &renderer,
                                            &console,
                                            &scheduler,
                                            &command_prompt,
                                        )?;
                                    }
                                    _ => {
                                        should_exit = self.apply_workspace_action(
                                            action,
                                            runtime,
                                            terminal_snapshot.size,
                                            &mut hosted,
                                            &tx,
                                            &mut console,
                                            &mut scheduler,
                                            &mut renderer_state,
                                            &renderer,
                                            &mut command_prompt,
                                        )?;
                                    }
                                }
                            } else if let Some(index) =
                                command_prompt.pick_session_index(&single, &self.sessions.list())
                            {
                                handled_control = true;
                                should_exit = self.apply_workspace_action(
                                    ConsoleAction::FocusIndex(index),
                                    runtime,
                                    terminal_snapshot.size,
                                    &mut hosted,
                                    &tx,
                                    &mut console,
                                    &mut scheduler,
                                    &mut renderer_state,
                                    &renderer,
                                    &mut command_prompt,
                                )?;
                            } else if command_prompt.submit_overlay(&single) {
                                handled_control = true;
                                if let Some(index) = command_prompt.selected_picker_index(
                                    &self.sessions.list(),
                                    console.focused_session.as_ref(),
                                ) {
                                    should_exit = self.apply_workspace_action(
                                        ConsoleAction::FocusIndex(index),
                                        runtime,
                                        terminal_snapshot.size,
                                        &mut hosted,
                                        &tx,
                                        &mut console,
                                        &mut scheduler,
                                        &mut renderer_state,
                                        &renderer,
                                        &mut command_prompt,
                                    )?;
                                } else {
                                    should_exit = self.apply_workspace_action(
                                        ConsoleAction::DismissOverlay,
                                        runtime,
                                        terminal_snapshot.size,
                                        &mut hosted,
                                        &tx,
                                        &mut console,
                                        &mut scheduler,
                                        &mut renderer_state,
                                        &renderer,
                                        &mut command_prompt,
                                    )?;
                                }
                            } else {
                                residual.push(byte);
                            }

                            if should_exit {
                                break;
                            }
                        }

                        if !should_exit {
                            let bytes_to_forward = if handled_control { residual } else { bytes };
                            if let Some(target) = console.input_owner_session().cloned() {
                                if let Some(runtime) = hosted.get_mut(&target) {
                                    if !bytes_to_forward.is_empty() {
                                        self.sessions.mark_input(&target);
                                        input_tracker.observe(
                                            &bytes_to_forward,
                                            &mut console,
                                            &mut scheduler,
                                            now_unix_ms(),
                                        );
                                        runtime.handle.write_all(&bytes_to_forward)?;
                                        self.render_workspace_console(
                                            &mut renderer_state,
                                            &renderer,
                                            &console,
                                            &scheduler,
                                            &command_prompt,
                                        )?;
                                    }
                                }
                            }
                        }
                    }
                }
                Ok(RuntimeEvent::InputClosed) => should_exit = true,
                Ok(RuntimeEvent::Output {
                    session: output_session,
                    bytes,
                }) => {
                    if let Some(runtime) = hosted.get_mut(&output_session) {
                        self.sessions.mark_output(&output_session);
                        runtime.screen_engine.feed(&bytes);
                        self.sessions
                            .update_screen_state(&output_session, runtime.screen_engine.state());
                        scheduler.on_session_output(&output_session, now_unix_ms());
                        self.render_workspace_console(
                            &mut renderer_state,
                            &renderer,
                            &console,
                            &scheduler,
                            &mut command_prompt,
                        )?;
                    }
                }
                Ok(RuntimeEvent::OutputClosed { session }) => {
                    if let Some(mut runtime) = hosted.remove(&session) {
                        let _ = runtime.handle.wait();
                        self.sessions.mark_exited(&session);
                        self.pty.release(&session);
                        let active_addresses = self.active_session_addresses();
                        console.handle_focus_loss(&active_addresses);
                        self.render_workspace_console(
                            &mut renderer_state,
                            &renderer,
                            &console,
                            &scheduler,
                            &mut command_prompt,
                        )?;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => should_exit = true,
            }

            if command_prompt.flush_picker_navigation_timeout(now_unix_ms()) {
                self.render_workspace_console(
                    &mut renderer_state,
                    &renderer,
                    &console,
                    &scheduler,
                    &command_prompt,
                )?;
            }

            if let Some(size) = self.terminal.capture_resize()? {
                for runtime in hosted.values_mut() {
                    runtime.handle.resize(PtySize::from(size))?;
                    runtime.screen_engine.resize(size);
                    let address = runtime.handle.ownership().session.clone();
                    self.sessions
                        .update_screen_state(&address, runtime.screen_engine.state());
                }
                self.render_workspace_console(
                    &mut renderer_state,
                    &renderer,
                    &console,
                    &scheduler,
                    &command_prompt,
                )?;
            }

            if !command_prompt.open {
                let decision =
                    scheduler.decide_auto_switch(&mut console, self.sessions.list(), now_unix_ms());
                let waiting_count = scheduler.waiting_queue().entries().len();
                if !matches!(decision.action, SchedulingAction::None)
                    || waiting_count != last_waiting_count
                {
                    self.render_workspace_console(
                        &mut renderer_state,
                        &renderer,
                        &console,
                        &scheduler,
                        &command_prompt,
                    )?;
                    last_waiting_count = waiting_count;
                }
            }
        }

        self.restore_terminal_screen()?;
        Ok(())
    }

    fn spawn_default_shell_session(
        &mut self,
        node_id: &str,
        size: crate::terminal::TerminalSize,
        hosted: &mut HashMap<SessionAddress, HostedSession>,
        tx: &Sender<RuntimeEvent>,
    ) -> Result<SessionAddress, AppError> {
        let program = default_shell_program();
        let title = shell_title(&program);
        self.spawn_managed_session(
            node_id.to_string(),
            title,
            program,
            Vec::new(),
            size,
            hosted,
            tx,
        )
    }

    fn spawn_managed_session(
        &mut self,
        node_id: String,
        title: String,
        program: String,
        args: Vec<String>,
        size: crate::terminal::TerminalSize,
        hosted: &mut HashMap<SessionAddress, HostedSession>,
        tx: &Sender<RuntimeEvent>,
    ) -> Result<SessionAddress, AppError> {
        let command_line = render_command_line(&program, &args);
        let session = self
            .sessions
            .create_local_session(node_id, title, command_line);
        let address = session.address().clone();
        let screen_engine = TerminalEngine::new(size);
        let handle = self.pty.spawn(
            address.clone(),
            SpawnRequest {
                program,
                args,
                size: PtySize::from(size),
            },
        )?;
        self.sessions.mark_running(&address, handle.process_id());
        self.sessions
            .update_screen_state(&address, screen_engine.state());
        spawn_pty_reader(handle.try_clone_reader()?, tx.clone(), address.clone());
        hosted.insert(
            address.clone(),
            HostedSession {
                handle,
                screen_engine,
            },
        );
        Ok(address)
    }

    fn handle_run(&mut self, command: RunCommand) -> Result<(), AppError> {
        print_banner();

        if command.program.is_empty() {
            return Err(AppError::InvalidCommand(
                "run requires an agent command".to_string(),
            ));
        }

        if let Some(connect_addr) = command.connect.clone() {
            let runtime = self
                .config
                .runtime_for_run(command.node_id.as_deref(), Some(connect_addr.as_str()));
            return self.delegate_run_to_server(&connect_addr, runtime.node.node_id, command);
        }

        let runtime = self
            .config
            .runtime_for_run(command.node_id.as_deref(), command.connect.as_deref());
        let command_line = command.command_line();
        let title = command.program.clone();
        let terminal_snapshot = self.terminal.snapshot()?;
        let session =
            self.sessions
                .create_local_session(runtime.node.node_id.clone(), title, command_line);
        let session_address = session.address().clone();
        let mut screen_engine = TerminalEngine::new(terminal_snapshot.size);
        let mut handle = self.pty.spawn(
            session_address.clone(),
            SpawnRequest {
                program: command.program,
                args: command.args,
                size: PtySize::from(terminal_snapshot.size),
            },
        )?;
        self.sessions
            .mark_running(&session_address, handle.process_id());
        self.sessions
            .update_screen_state(&session_address, screen_engine.state());

        if terminal_snapshot.input_is_tty && terminal_snapshot.output_is_tty {
            return self.run_single_session_passthrough(session_address, &mut handle);
        }

        print_runtime_header("run", &runtime, Some(session.address()));
        println!("agent_command: {}", session.command_line);
        println!("pty_id: {}", handle.pty_id());
        println!("status: running");
        println!(
            "terminal_size: {}x{}",
            handle.size().cols,
            handle.size().rows
        );
        println!(
            "console_tty: input={}, output={}",
            terminal_snapshot.input_is_tty, terminal_snapshot.output_is_tty
        );
        if let Some(process_id) = handle.process_id() {
            println!("process_id: {process_id}");
        }
        if let Some(connect_addr) = runtime.network.access_point.as_deref() {
            println!("mirror: enabled");
            println!("mirror_target: {connect_addr}");
        } else {
            println!("mirror: disabled");
        }
        println!(
            "note: interactive console runtime is enabled automatically when stdin/stdout are attached to a TTY."
        );
        println!();

        let output = handle.read_to_end()?;
        if !output.is_empty() {
            self.sessions.mark_output(&session_address);
            screen_engine.feed(&output);
            self.sessions
                .update_screen_state(&session_address, screen_engine.state());
            let mut stdout = io::stdout().lock();
            stdout
                .write_all(&output)
                .map_err(|error| AppError::Io("failed to write PTY output".to_string(), error))?;
            stdout
                .flush()
                .map_err(|error| AppError::Io("failed to flush PTY output".to_string(), error))?;
        }

        self.finish_session(&mut handle, &session_address)
    }

    #[allow(dead_code)]
    fn run_local_console(
        &mut self,
        session: SessionAddress,
        handle: &mut PtyHandle,
        screen_engine: &mut TerminalEngine,
    ) -> Result<(), AppError> {
        let mut console = ConsoleState::new("local-console");
        let addresses = self
            .sessions
            .list()
            .into_iter()
            .map(|record| record.address().clone())
            .collect::<Vec<_>>();
        console.select_initial_focus(&addresses);

        let _raw_mode = self.terminal.enter_raw_mode()?;
        let mut scheduler = SchedulerState::new();
        let renderer = Renderer::new();
        let mut renderer_state = RendererState::default();
        let mut input_tracker = InputTracker::default();

        let (tx, rx) = mpsc::channel();
        spawn_stdin_reader(tx.clone());
        spawn_pty_reader(handle.try_clone_reader()?, tx, session.clone());

        self.render_console(
            &mut renderer_state,
            &renderer,
            &console,
            &scheduler,
            Vec::new(),
            None,
        )?;
        let mut last_waiting_count = scheduler.waiting_queue().entries().len();

        let mut process_closed = false;
        loop {
            match rx.recv_timeout(EVENT_LOOP_TICK) {
                Ok(RuntimeEvent::Input(bytes)) => {
                    if let Some(target) = console.input_owner_session().cloned() {
                        self.sessions.mark_input(&target);
                        input_tracker.observe(&bytes, &mut console, &mut scheduler, now_unix_ms());
                        handle.write_all(&bytes)?;
                        self.render_console(
                            &mut renderer_state,
                            &renderer,
                            &console,
                            &scheduler,
                            Vec::new(),
                            None,
                        )?;
                    }
                }
                Ok(RuntimeEvent::InputClosed) => {}
                Ok(RuntimeEvent::Output {
                    session: output_session,
                    bytes,
                }) => {
                    self.sessions.mark_output(&output_session);
                    screen_engine.feed(&bytes);
                    self.sessions
                        .update_screen_state(&output_session, screen_engine.state());
                    scheduler.on_session_output(&output_session, now_unix_ms());
                    self.render_console(
                        &mut renderer_state,
                        &renderer,
                        &console,
                        &scheduler,
                        Vec::new(),
                        None,
                    )?;
                }
                Ok(RuntimeEvent::OutputClosed { session: closed }) => {
                    if closed == session {
                        process_closed = true;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    process_closed = true;
                }
            }

            if let Some(size) = self.terminal.capture_resize()? {
                handle.resize(PtySize::from(size))?;
                screen_engine.resize(size);
                self.sessions
                    .update_screen_state(&session, screen_engine.state());
                self.render_console(
                    &mut renderer_state,
                    &renderer,
                    &console,
                    &scheduler,
                    Vec::new(),
                    None,
                )?;
            }

            let decision =
                scheduler.decide_auto_switch(&mut console, self.sessions.list(), now_unix_ms());
            let waiting_count = scheduler.waiting_queue().entries().len();
            if !matches!(decision.action, SchedulingAction::None)
                || waiting_count != last_waiting_count
            {
                self.render_console(
                    &mut renderer_state,
                    &renderer,
                    &console,
                    &scheduler,
                    Vec::new(),
                    None,
                )?;
                last_waiting_count = waiting_count;
            }

            if process_closed {
                break;
            }
        }

        self.restore_terminal_screen()?;
        self.finish_session(handle, &session)
    }

    fn run_single_session_passthrough(
        &mut self,
        session: SessionAddress,
        handle: &mut PtyHandle,
    ) -> Result<(), AppError> {
        let _raw_mode = self.terminal.enter_raw_mode()?;
        let (tx, rx) = mpsc::channel();
        spawn_stdin_reader(tx.clone());
        spawn_pty_reader(handle.try_clone_reader()?, tx, session.clone());

        let mut process_closed = false;
        while !process_closed {
            match rx.recv_timeout(EVENT_LOOP_TICK) {
                Ok(RuntimeEvent::Input(bytes)) => {
                    self.sessions.mark_input(&session);
                    handle.write_all(&bytes)?;
                }
                Ok(RuntimeEvent::InputClosed) => {}
                Ok(RuntimeEvent::Output {
                    session: output_session,
                    bytes,
                }) => {
                    self.sessions.mark_output(&output_session);
                    let mut stdout = io::stdout().lock();
                    stdout.write_all(&bytes).map_err(|error| {
                        AppError::Io("failed to write PTY passthrough output".to_string(), error)
                    })?;
                    stdout.flush().map_err(|error| {
                        AppError::Io("failed to flush PTY passthrough output".to_string(), error)
                    })?;
                }
                Ok(RuntimeEvent::OutputClosed { session: closed }) => {
                    if closed == session {
                        process_closed = true;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    process_closed = true;
                }
            }

            if let Some(size) = self.terminal.capture_resize()? {
                handle.resize(PtySize::from(size))?;
            }
        }

        self.finish_session(handle, &session)
    }

    fn render_console(
        &self,
        renderer_state: &mut RendererState,
        renderer: &Renderer,
        console: &ConsoleState,
        scheduler: &SchedulerState,
        overlay_lines: Vec<String>,
        command_prompt: Option<&CommandPromptState>,
    ) -> Result<(), AppError> {
        let frame = renderer.render_with_state(
            renderer_state,
            console,
            &self.sessions.list(),
            RenderContext {
                waiting_count: scheduler.waiting_queue().entries().len(),
                overlay_lines,
            },
        )?;
        let (frame_text, cursor) = self.decorate_frame(&frame, command_prompt);
        self.write_full_frame_at(&frame_text, cursor)
    }

    fn decorate_frame(
        &self,
        frame: &RenderFrame,
        command_prompt: Option<&CommandPromptState>,
    ) -> (String, CursorPlacement) {
        let width = self.terminal.current_size_or_default().cols as usize;
        let mut lines =
            Vec::with_capacity(frame.viewport_lines.len() + frame.overlay_lines.len() + 2);
        if !frame.top_line.is_empty() {
            lines.push(frame.top_line.clone());
        }
        lines.extend(frame.viewport_lines.iter().cloned());
        lines.extend(
            frame
                .overlay_lines
                .iter()
                .map(|line| style_overlay_line(line, width)),
        );
        lines.push(style_status_line(&frame.bottom_line, width));
        let cursor = command_prompt
            .filter(|prompt| prompt.open)
            .map(|prompt| self.command_bar_cursor(frame, prompt))
            .unwrap_or_else(|| self.frame_cursor(frame));
        (lines.join("\r\n"), cursor)
    }

    fn write_full_frame(&self, frame_text: &str) -> Result<(), AppError> {
        self.write_full_frame_at(frame_text, CursorPlacement { row: 0, col: 0 })
    }

    fn frame_cursor(&self, frame: &RenderFrame) -> CursorPlacement {
        let row_offset = u16::from(!frame.top_line.is_empty());
        CursorPlacement {
            row: row_offset.saturating_add(frame.cursor_row),
            col: frame.cursor_col,
        }
    }

    fn command_bar_cursor(
        &self,
        frame: &RenderFrame,
        command_prompt: &CommandPromptState,
    ) -> CursorPlacement {
        let command_row = frame
            .viewport_lines
            .len()
            .saturating_add(frame.overlay_lines.len().saturating_sub(1))
            as u16;
        let max_col = self
            .terminal
            .current_size_or_default()
            .cols
            .saturating_sub(1);
        let command_col = (1 + command_prompt.buffer.chars().count()) as u16;
        CursorPlacement {
            row: command_row,
            col: command_col.min(max_col),
        }
    }

    fn write_full_frame_at(
        &self,
        frame_text: &str,
        cursor: CursorPlacement,
    ) -> Result<(), AppError> {
        let mut stdout = io::stdout().lock();
        stdout
            .write_all(CLEAR_FRAME.as_bytes())
            .map_err(|error| AppError::Io("failed to clear terminal frame".to_string(), error))?;
        stdout
            .write_all(frame_text.as_bytes())
            .map_err(|error| AppError::Io("failed to write render frame".to_string(), error))?;
        stdout
            .write_all(
                format!(
                    "\x1b[{};{}H\x1b[?25h",
                    cursor.row.saturating_add(1),
                    cursor.col.saturating_add(1)
                )
                .as_bytes(),
            )
            .map_err(|error| AppError::Io("failed to position render cursor".to_string(), error))?;
        stdout
            .flush()
            .map_err(|error| AppError::Io("failed to flush render frame".to_string(), error))?;
        Ok(())
    }

    fn restore_terminal_screen(&self) -> Result<(), AppError> {
        let mut stdout = io::stdout().lock();
        stdout
            .write_all(RESTORE_SCREEN.as_bytes())
            .map_err(|error| {
                AppError::Io("failed to restore terminal screen".to_string(), error)
            })?;
        stdout
            .flush()
            .map_err(|error| AppError::Io("failed to flush terminal restore".to_string(), error))?;
        Ok(())
    }

    fn finish_session(
        &mut self,
        handle: &mut PtyHandle,
        session: &SessionAddress,
    ) -> Result<(), AppError> {
        let exit_status = handle.wait()?;
        self.sessions.mark_exited(session);
        self.pty.release(session);
        if !exit_status.success() {
            println!();
            println!("session_exit: {}", format_exit_status(exit_status));
        }
        Ok(())
    }

    fn delegate_run_to_server(
        &mut self,
        connect_addr: &str,
        node_id: String,
        command: RunCommand,
    ) -> Result<(), AppError> {
        let request = DelegatedSpawnRequest {
            node_id,
            program: command.program,
            args: command.args,
        };
        let mut client = ClientRuntime::connect(ClientRuntimeConfig::new(
            connect_addr.to_string(),
            request.node_id.clone(),
        ))?;
        let _server_hello = client.register_node(0, None)?;
        let acceptance = client.delegate_spawn(&request)?;

        println!("waitagent_command: run");
        println!("mode: delegated");
        println!("access_point: {}", client.endpoint());
        println!("session: {}", acceptance.session_address);
        println!("status: accepted");
        println!(
            "note: delegated spawns are hosted by the connected WaitAgent server; mirrored local IO remains future work."
        );
        Ok(())
    }

    fn run_local_host(&mut self, runtime: &AppConfig) -> Result<(), AppError> {
        let terminal_snapshot = self.terminal.snapshot()?;
        if !terminal_snapshot.input_is_tty || !terminal_snapshot.output_is_tty {
            return Err(AppError::Terminal(crate::terminal::TerminalError::NotTty(
                "server console".to_string(),
            )));
        }

        let listen_addr = normalize_endpoint(&runtime.network.listen_addr);
        let mut server_runtime =
            ServerRuntime::bind(ServerRuntimeConfig::new(listen_addr.clone()))?;

        let _alternate_screen = self.terminal.enter_alternate_screen()?;
        let _raw_mode = self.terminal.enter_raw_mode()?;
        let mut console = ConsoleState::new("server-console");
        let mut scheduler = SchedulerState::new();
        let renderer = Renderer::new();
        let mut renderer_state = RendererState::default();
        let mut input_tracker = InputTracker::default();
        let mut command_prompt = CommandPromptState::default();
        let mut hosted = HashMap::<SessionAddress, HostedSession>::new();

        let (tx, rx) = mpsc::channel();
        spawn_stdin_reader(tx.clone());

        self.render_host_console(
            &mut renderer_state,
            &renderer,
            &console,
            &scheduler,
            &command_prompt,
        )?;
        let mut last_waiting_count = scheduler.waiting_queue().entries().len();
        let mut should_exit = false;

        while !should_exit {
            for mut connection in server_runtime.accept_pending()? {
                let registration = register_client_connection(&mut server_runtime, &mut connection);
                if let Err(error) = registration {
                    let _ = write_delegated_spawn_response(
                        &mut connection.stream,
                        Err(error.to_string()),
                    );
                    continue;
                }

                match read_delegated_spawn_request(&mut connection.stream) {
                    Ok(request) => {
                        let address = self.spawn_hosted_session(
                            request,
                            terminal_snapshot.size,
                            &mut hosted,
                            &tx,
                        )?;
                        let active_addresses = self.active_session_addresses();
                        console.select_initial_focus(&active_addresses);
                        write_delegated_spawn_response(
                            &mut connection.stream,
                            Ok(address.to_string()),
                        )?;
                        self.render_host_console(
                            &mut renderer_state,
                            &renderer,
                            &console,
                            &scheduler,
                            &mut command_prompt,
                        )?;
                    }
                    Err(error) => {
                        let _ = write_delegated_spawn_response(
                            &mut connection.stream,
                            Err(error.to_string()),
                        );
                    }
                }
            }

            match rx.recv_timeout(EVENT_LOOP_TICK) {
                Ok(RuntimeEvent::Input(bytes)) => {
                    let input_received_at = now_unix_ms();
                    if let Some(outcome) = command_prompt.handle_picker_navigation(
                        &bytes,
                        &self.sessions.list(),
                        console.focused_session.as_ref(),
                        input_received_at,
                    ) {
                        if matches!(outcome, PickerNavigationOutcome::Render) {
                            self.render_host_console(
                                &mut renderer_state,
                                &renderer,
                                &console,
                                &scheduler,
                                &command_prompt,
                            )?;
                        }
                    } else if matches!(parse_console_action(&bytes), Some(ConsoleAction::QuitHost))
                    {
                        should_exit = true;
                    } else if let Some(outcome) = command_prompt.handle_input(&bytes) {
                        should_exit = self.apply_command_outcome(
                            outcome,
                            runtime,
                            terminal_snapshot.size,
                            &mut hosted,
                            &tx,
                            &mut console,
                            &mut scheduler,
                            &mut renderer_state,
                            &renderer,
                            &mut command_prompt,
                            RenderSurface::Server,
                        )?;
                    } else if let Some(action) = parse_console_action(&bytes) {
                        match action {
                            ConsoleAction::PreviousSession
                                if command_prompt.move_picker_previous(
                                    &self.sessions.list(),
                                    console.focused_session.as_ref(),
                                ) =>
                            {
                                self.render_host_console(
                                    &mut renderer_state,
                                    &renderer,
                                    &console,
                                    &scheduler,
                                    &command_prompt,
                                )?;
                            }
                            ConsoleAction::NextSession
                                if command_prompt.move_picker_next(
                                    &self.sessions.list(),
                                    console.focused_session.as_ref(),
                                ) =>
                            {
                                self.render_host_console(
                                    &mut renderer_state,
                                    &renderer,
                                    &console,
                                    &scheduler,
                                    &command_prompt,
                                )?;
                            }
                            _ => {
                                should_exit = self.apply_host_action(
                                    action,
                                    runtime,
                                    terminal_snapshot.size,
                                    &mut hosted,
                                    &tx,
                                    &mut console,
                                    &mut scheduler,
                                    &mut renderer_state,
                                    &renderer,
                                    &mut command_prompt,
                                )?;
                            }
                        }
                    } else if let Some(index) =
                        command_prompt.pick_session_index(&bytes, &self.sessions.list())
                    {
                        should_exit = self.apply_host_action(
                            ConsoleAction::FocusIndex(index),
                            runtime,
                            terminal_snapshot.size,
                            &mut hosted,
                            &tx,
                            &mut console,
                            &mut scheduler,
                            &mut renderer_state,
                            &renderer,
                            &mut command_prompt,
                        )?;
                    } else if command_prompt.submit_overlay(&bytes) {
                        if let Some(index) = command_prompt.selected_picker_index(
                            &self.sessions.list(),
                            console.focused_session.as_ref(),
                        ) {
                            should_exit = self.apply_host_action(
                                ConsoleAction::FocusIndex(index),
                                runtime,
                                terminal_snapshot.size,
                                &mut hosted,
                                &tx,
                                &mut console,
                                &mut scheduler,
                                &mut renderer_state,
                                &renderer,
                                &mut command_prompt,
                            )?;
                        } else {
                            should_exit = self.apply_host_action(
                                ConsoleAction::DismissOverlay,
                                runtime,
                                terminal_snapshot.size,
                                &mut hosted,
                                &tx,
                                &mut console,
                                &mut scheduler,
                                &mut renderer_state,
                                &renderer,
                                &mut command_prompt,
                            )?;
                        }
                    } else {
                        let mut residual = Vec::new();
                        let mut handled_control = false;

                        for &byte in &bytes {
                            let single = [byte];
                            if let Some(outcome) = command_prompt.handle_picker_navigation(
                                &single,
                                &self.sessions.list(),
                                console.focused_session.as_ref(),
                                now_unix_ms(),
                            ) {
                                handled_control = true;
                                if matches!(outcome, PickerNavigationOutcome::Render) {
                                    self.render_host_console(
                                        &mut renderer_state,
                                        &renderer,
                                        &console,
                                        &scheduler,
                                        &command_prompt,
                                    )?;
                                }
                            } else if let Some(outcome) = command_prompt.handle_input(&single) {
                                handled_control = true;
                                should_exit = self.apply_command_outcome(
                                    outcome,
                                    runtime,
                                    terminal_snapshot.size,
                                    &mut hosted,
                                    &tx,
                                    &mut console,
                                    &mut scheduler,
                                    &mut renderer_state,
                                    &renderer,
                                    &mut command_prompt,
                                    RenderSurface::Server,
                                )?;
                            } else if let Some(action) = parse_console_action(&single) {
                                handled_control = true;
                                match action {
                                    ConsoleAction::PreviousSession
                                        if command_prompt.move_picker_previous(
                                            &self.sessions.list(),
                                            console.focused_session.as_ref(),
                                        ) =>
                                    {
                                        self.render_host_console(
                                            &mut renderer_state,
                                            &renderer,
                                            &console,
                                            &scheduler,
                                            &command_prompt,
                                        )?;
                                    }
                                    ConsoleAction::NextSession
                                        if command_prompt.move_picker_next(
                                            &self.sessions.list(),
                                            console.focused_session.as_ref(),
                                        ) =>
                                    {
                                        self.render_host_console(
                                            &mut renderer_state,
                                            &renderer,
                                            &console,
                                            &scheduler,
                                            &command_prompt,
                                        )?;
                                    }
                                    _ => {
                                        should_exit = self.apply_host_action(
                                            action,
                                            runtime,
                                            terminal_snapshot.size,
                                            &mut hosted,
                                            &tx,
                                            &mut console,
                                            &mut scheduler,
                                            &mut renderer_state,
                                            &renderer,
                                            &mut command_prompt,
                                        )?;
                                    }
                                }
                            } else if let Some(index) =
                                command_prompt.pick_session_index(&single, &self.sessions.list())
                            {
                                handled_control = true;
                                should_exit = self.apply_host_action(
                                    ConsoleAction::FocusIndex(index),
                                    runtime,
                                    terminal_snapshot.size,
                                    &mut hosted,
                                    &tx,
                                    &mut console,
                                    &mut scheduler,
                                    &mut renderer_state,
                                    &renderer,
                                    &mut command_prompt,
                                )?;
                            } else if command_prompt.submit_overlay(&single) {
                                handled_control = true;
                                if let Some(index) = command_prompt.selected_picker_index(
                                    &self.sessions.list(),
                                    console.focused_session.as_ref(),
                                ) {
                                    should_exit = self.apply_host_action(
                                        ConsoleAction::FocusIndex(index),
                                        runtime,
                                        terminal_snapshot.size,
                                        &mut hosted,
                                        &tx,
                                        &mut console,
                                        &mut scheduler,
                                        &mut renderer_state,
                                        &renderer,
                                        &mut command_prompt,
                                    )?;
                                } else {
                                    should_exit = self.apply_host_action(
                                        ConsoleAction::DismissOverlay,
                                        runtime,
                                        terminal_snapshot.size,
                                        &mut hosted,
                                        &tx,
                                        &mut console,
                                        &mut scheduler,
                                        &mut renderer_state,
                                        &renderer,
                                        &mut command_prompt,
                                    )?;
                                }
                            } else {
                                residual.push(byte);
                            }

                            if should_exit {
                                break;
                            }
                        }

                        if !should_exit {
                            let bytes_to_forward = if handled_control { residual } else { bytes };
                            if let Some(target) = console.input_owner_session().cloned() {
                                if let Some(runtime) = hosted.get_mut(&target) {
                                    if !bytes_to_forward.is_empty() {
                                        self.sessions.mark_input(&target);
                                        input_tracker.observe(
                                            &bytes_to_forward,
                                            &mut console,
                                            &mut scheduler,
                                            now_unix_ms(),
                                        );
                                        runtime.handle.write_all(&bytes_to_forward)?;
                                        self.render_host_console(
                                            &mut renderer_state,
                                            &renderer,
                                            &console,
                                            &scheduler,
                                            &command_prompt,
                                        )?;
                                    }
                                }
                            }
                        }
                    }
                }
                Ok(RuntimeEvent::InputClosed) => should_exit = true,
                Ok(RuntimeEvent::Output {
                    session: output_session,
                    bytes,
                }) => {
                    if let Some(runtime) = hosted.get_mut(&output_session) {
                        self.sessions.mark_output(&output_session);
                        runtime.screen_engine.feed(&bytes);
                        self.sessions
                            .update_screen_state(&output_session, runtime.screen_engine.state());
                        scheduler.on_session_output(&output_session, now_unix_ms());
                        self.render_host_console(
                            &mut renderer_state,
                            &renderer,
                            &console,
                            &scheduler,
                            &mut command_prompt,
                        )?;
                    }
                }
                Ok(RuntimeEvent::OutputClosed { session }) => {
                    if let Some(mut runtime) = hosted.remove(&session) {
                        let _ = runtime.handle.wait();
                        self.sessions.mark_exited(&session);
                        self.pty.release(&session);
                        let active_addresses = self.active_session_addresses();
                        console.handle_focus_loss(&active_addresses);
                        self.render_host_console(
                            &mut renderer_state,
                            &renderer,
                            &console,
                            &scheduler,
                            &command_prompt,
                        )?;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => should_exit = true,
            }

            if command_prompt.flush_picker_navigation_timeout(now_unix_ms()) {
                self.render_host_console(
                    &mut renderer_state,
                    &renderer,
                    &console,
                    &scheduler,
                    &command_prompt,
                )?;
            }

            server_runtime.expire_stale_nodes(now_unix_ms());

            if let Some(size) = self.terminal.capture_resize()? {
                for runtime in hosted.values_mut() {
                    runtime.handle.resize(PtySize::from(size))?;
                    runtime.screen_engine.resize(size);
                    let address = runtime.handle.ownership().session.clone();
                    self.sessions
                        .update_screen_state(&address, runtime.screen_engine.state());
                }
                self.render_host_console(
                    &mut renderer_state,
                    &renderer,
                    &console,
                    &scheduler,
                    &command_prompt,
                )?;
            }

            if !command_prompt.open {
                let decision =
                    scheduler.decide_auto_switch(&mut console, self.sessions.list(), now_unix_ms());
                let waiting_count = scheduler.waiting_queue().entries().len();
                if !matches!(decision.action, SchedulingAction::None)
                    || waiting_count != last_waiting_count
                {
                    self.render_host_console(
                        &mut renderer_state,
                        &renderer,
                        &console,
                        &scheduler,
                        &command_prompt,
                    )?;
                    last_waiting_count = waiting_count;
                }
            }
        }

        self.restore_terminal_screen()?;
        Ok(())
    }

    fn spawn_hosted_session(
        &mut self,
        request: DelegatedSpawnRequest,
        size: crate::terminal::TerminalSize,
        hosted: &mut HashMap<SessionAddress, HostedSession>,
        tx: &Sender<RuntimeEvent>,
    ) -> Result<SessionAddress, AppError> {
        self.spawn_managed_session(
            request.node_id,
            request.program.clone(),
            request.program,
            request.args,
            size,
            hosted,
            tx,
        )
    }

    fn active_session_addresses(&self) -> Vec<SessionAddress> {
        self.sessions
            .list()
            .into_iter()
            .filter(|record| !matches!(record.status, SessionStatus::Exited))
            .map(|record| record.address().clone())
            .collect()
    }

    fn render_workspace_console(
        &self,
        renderer_state: &mut RendererState,
        renderer: &Renderer,
        console: &ConsoleState,
        scheduler: &SchedulerState,
        command_prompt: &CommandPromptState,
    ) -> Result<(), AppError> {
        let overlay_lines =
            command_prompt.overlay_lines(self.sessions.list(), console.focused_session.as_ref());
        if console.focused_session.is_none() {
            let active_count = self
                .sessions
                .list()
                .into_iter()
                .filter(|record| !matches!(record.status, SessionStatus::Exited))
                .count();
            let waiting_count = scheduler.waiting_queue().entries().len();
            let idle_frame = self.render_idle_frame(
                "workspace",
                active_count,
                waiting_count,
                &overlay_lines,
                "focus: none | mode: workspace-idle",
            );
            self.write_full_frame(&idle_frame)?;
            return Ok(());
        }

        self.render_console(
            renderer_state,
            renderer,
            console,
            scheduler,
            overlay_lines,
            Some(command_prompt),
        )
    }

    fn render_host_console(
        &self,
        renderer_state: &mut RendererState,
        renderer: &Renderer,
        console: &ConsoleState,
        scheduler: &SchedulerState,
        command_prompt: &CommandPromptState,
    ) -> Result<(), AppError> {
        let overlay_lines =
            command_prompt.overlay_lines(self.sessions.list(), console.focused_session.as_ref());
        if console.focused_session.is_none() {
            let active_count = self
                .sessions
                .list()
                .into_iter()
                .filter(|record| !matches!(record.status, SessionStatus::Exited))
                .count();
            let waiting_count = scheduler.waiting_queue().entries().len();
            let idle_frame = self.render_idle_frame(
                "host",
                active_count,
                waiting_count,
                &overlay_lines,
                "focus: none | mode: host-idle",
            );
            self.write_full_frame(&idle_frame)?;
            return Ok(());
        }

        self.render_console(
            renderer_state,
            renderer,
            console,
            scheduler,
            overlay_lines,
            Some(command_prompt),
        )
    }

    fn apply_host_action(
        &mut self,
        action: ConsoleAction,
        runtime: &AppConfig,
        size: crate::terminal::TerminalSize,
        hosted: &mut HashMap<SessionAddress, HostedSession>,
        tx: &Sender<RuntimeEvent>,
        console: &mut ConsoleState,
        scheduler: &mut SchedulerState,
        renderer_state: &mut RendererState,
        renderer: &Renderer,
        command_prompt: &mut CommandPromptState,
    ) -> Result<bool, AppError> {
        let active_addresses = self.active_session_addresses();
        let changed = match action {
            ConsoleAction::CreateSession => {
                let address =
                    self.spawn_default_shell_session(&runtime.node.node_id, size, hosted, tx)?;
                console.focus(address);
                command_prompt.set_message("Created new session.");
                true
            }
            ConsoleAction::ListSessions => {
                command_prompt
                    .toggle_sessions(&self.sessions.list(), console.focused_session.as_ref());
                true
            }
            ConsoleAction::DismissOverlay => command_prompt.dismiss(),
            ConsoleAction::CloseCurrentSession => {
                if let Some(target) = console.focused_session.clone() {
                    let closed = self.close_managed_session(&target, hosted, console, scheduler)?;
                    if closed {
                        command_prompt.set_message(format!("Closed {target}."));
                    } else {
                        command_prompt.set_message("No active session to close.");
                    }
                    closed
                } else {
                    command_prompt.set_message("No focused session to close.");
                    false
                }
            }
            ConsoleAction::NextSession => {
                let changed = console.focus_next(&active_addresses).is_some();
                if changed && !matches!(command_prompt.overlay, CommandOverlay::Sessions) {
                    command_prompt.clear_overlay();
                }
                changed
            }
            ConsoleAction::PreviousSession => {
                let changed = console.focus_previous(&active_addresses).is_some();
                if changed && !matches!(command_prompt.overlay, CommandOverlay::Sessions) {
                    command_prompt.clear_overlay();
                }
                changed
            }
            ConsoleAction::FocusIndex(index) => {
                let changed = console.focus_index(&active_addresses, index).is_some();
                if changed {
                    command_prompt.clear_overlay();
                }
                changed
            }
            ConsoleAction::TogglePeek => {
                if console.is_peeking() {
                    console.exit_peek().is_some()
                } else if let Some(target) = active_addresses
                    .iter()
                    .find(|address| Some(*address) != console.focused_session.as_ref())
                {
                    console.enter_peek(&active_addresses, target).is_some()
                } else {
                    false
                }
            }
            ConsoleAction::QuitHost => return Ok(true),
        };

        if changed {
            if !matches!(action, ConsoleAction::TogglePeek) {
                scheduler.on_manual_switch(console);
            }
            self.render_host_console(renderer_state, renderer, console, scheduler, command_prompt)?;
        }

        Ok(false)
    }

    fn apply_workspace_action(
        &mut self,
        action: ConsoleAction,
        runtime: &AppConfig,
        size: crate::terminal::TerminalSize,
        hosted: &mut HashMap<SessionAddress, HostedSession>,
        tx: &Sender<RuntimeEvent>,
        console: &mut ConsoleState,
        scheduler: &mut SchedulerState,
        renderer_state: &mut RendererState,
        renderer: &Renderer,
        command_prompt: &mut CommandPromptState,
    ) -> Result<bool, AppError> {
        let active_addresses = self.active_session_addresses();
        let changed = match action {
            ConsoleAction::CreateSession => {
                let address =
                    self.spawn_default_shell_session(&runtime.node.node_id, size, hosted, tx)?;
                console.focus(address);
                command_prompt.set_message("Created new session.");
                true
            }
            ConsoleAction::ListSessions => {
                command_prompt
                    .toggle_sessions(&self.sessions.list(), console.focused_session.as_ref());
                true
            }
            ConsoleAction::DismissOverlay => command_prompt.dismiss(),
            ConsoleAction::CloseCurrentSession => {
                if let Some(target) = console.focused_session.clone() {
                    let closed = self.close_managed_session(&target, hosted, console, scheduler)?;
                    if closed {
                        command_prompt.set_message(format!("Closed {target}."));
                    } else {
                        command_prompt.set_message("No active session to close.");
                    }
                    closed
                } else {
                    command_prompt.set_message("No focused session to close.");
                    false
                }
            }
            ConsoleAction::NextSession => {
                let changed = console.focus_next(&active_addresses).is_some();
                if changed && !matches!(command_prompt.overlay, CommandOverlay::Sessions) {
                    command_prompt.clear_overlay();
                }
                changed
            }
            ConsoleAction::PreviousSession => {
                let changed = console.focus_previous(&active_addresses).is_some();
                if changed && !matches!(command_prompt.overlay, CommandOverlay::Sessions) {
                    command_prompt.clear_overlay();
                }
                changed
            }
            ConsoleAction::FocusIndex(index) => {
                let changed = console.focus_index(&active_addresses, index).is_some();
                if changed {
                    command_prompt.clear_overlay();
                }
                changed
            }
            ConsoleAction::TogglePeek => {
                if console.is_peeking() {
                    console.exit_peek().is_some()
                } else if let Some(target) = active_addresses
                    .iter()
                    .find(|address| Some(*address) != console.focused_session.as_ref())
                {
                    console.enter_peek(&active_addresses, target).is_some()
                } else {
                    false
                }
            }
            ConsoleAction::QuitHost => return Ok(true),
        };

        if changed {
            if !matches!(action, ConsoleAction::TogglePeek) {
                scheduler.on_manual_switch(console);
            }
            self.render_workspace_console(
                renderer_state,
                renderer,
                console,
                scheduler,
                command_prompt,
            )?;
        }

        Ok(false)
    }

    fn apply_command_outcome(
        &mut self,
        outcome: CommandPromptOutcome,
        runtime: &AppConfig,
        size: crate::terminal::TerminalSize,
        hosted: &mut HashMap<SessionAddress, HostedSession>,
        tx: &Sender<RuntimeEvent>,
        console: &mut ConsoleState,
        scheduler: &mut SchedulerState,
        renderer_state: &mut RendererState,
        renderer: &Renderer,
        command_prompt: &mut CommandPromptState,
        surface: RenderSurface,
    ) -> Result<bool, AppError> {
        match outcome {
            CommandPromptOutcome::RenderOnly => {
                self.render_surface(
                    surface,
                    renderer_state,
                    renderer,
                    console,
                    scheduler,
                    command_prompt,
                )?;
                Ok(false)
            }
            CommandPromptOutcome::Execute(command) => self.execute_command_prompt(
                command.as_str(),
                runtime,
                size,
                hosted,
                tx,
                console,
                scheduler,
                renderer_state,
                renderer,
                command_prompt,
                surface,
            ),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn execute_command_prompt(
        &mut self,
        command: &str,
        runtime: &AppConfig,
        size: crate::terminal::TerminalSize,
        hosted: &mut HashMap<SessionAddress, HostedSession>,
        tx: &Sender<RuntimeEvent>,
        console: &mut ConsoleState,
        scheduler: &mut SchedulerState,
        renderer_state: &mut RendererState,
        renderer: &Renderer,
        command_prompt: &mut CommandPromptState,
        surface: RenderSurface,
    ) -> Result<bool, AppError> {
        let trimmed = command.trim();
        let mut should_exit = false;

        if trimmed.is_empty() {
            command_prompt.set_message("Empty command. Try /h.");
        } else if matches!(trimmed, "/h" | "/help") {
            command_prompt.toggle_help();
        } else if trimmed == "/new" {
            let address =
                self.spawn_default_shell_session(&runtime.node.node_id, size, hosted, tx)?;
            console.focus(address.clone());
            scheduler.on_manual_switch(console);
            command_prompt.set_message(format!("Created {address}."));
        } else if matches!(trimmed, "/sessions" | "/ls") {
            command_prompt.toggle_sessions(&self.sessions.list(), console.focused_session.as_ref());
        } else if matches!(trimmed, "/clear" | "/dismiss") {
            command_prompt.clear_overlay();
        } else if matches!(trimmed, "/close" | "/kill") {
            if let Some(target) = console.focused_session.clone() {
                if self.close_managed_session(&target, hosted, console, scheduler)? {
                    command_prompt.set_message(format!("Closed {target}."));
                } else {
                    command_prompt.set_message("No active session to close.");
                }
            } else {
                command_prompt.set_message("No focused session to close.");
            }
        } else if matches!(trimmed, "/quit" | "/q") {
            should_exit = true;
        } else if let Some(argument) = trimmed.strip_prefix("/focus ") {
            let changed = self.focus_command_target(argument.trim(), console, scheduler);
            if changed {
                command_prompt.set_message(format!(
                    "Focused {}.",
                    console
                        .focused_session
                        .as_ref()
                        .map(ToString::to_string)
                        .unwrap_or_else(|| "none".to_string())
                ));
            } else {
                command_prompt.set_message(format!("Could not focus `{}`.", argument.trim()));
            }
        } else if trimmed == "/next" {
            if console
                .focus_next(&self.active_session_addresses())
                .is_some()
            {
                scheduler.on_manual_switch(console);
                command_prompt.set_message("Moved to next session.");
            } else {
                command_prompt.set_message("No next session available.");
            }
        } else if trimmed == "/prev" {
            if console
                .focus_previous(&self.active_session_addresses())
                .is_some()
            {
                scheduler.on_manual_switch(console);
                command_prompt.set_message("Moved to previous session.");
            } else {
                command_prompt.set_message("No previous session available.");
            }
        } else {
            command_prompt.set_message(format!("Unknown command: {trimmed}"));
        }

        self.render_surface(
            surface,
            renderer_state,
            renderer,
            console,
            scheduler,
            command_prompt,
        )?;
        Ok(should_exit)
    }

    fn focus_command_target(
        &mut self,
        target: &str,
        console: &mut ConsoleState,
        scheduler: &mut SchedulerState,
    ) -> bool {
        let addresses = self.active_session_addresses();
        let changed = if let Ok(index) = target.parse::<usize>() {
            console.focus_index(&addresses, index).is_some()
        } else {
            let matches = addresses
                .iter()
                .find(|address| address.session_id() == target || address.to_string() == target)
                .cloned();
            matches
                .as_ref()
                .and_then(|address| console.focus_address(&addresses, address))
                .is_some()
        };

        if changed {
            scheduler.on_manual_switch(console);
        }

        changed
    }

    fn close_managed_session(
        &mut self,
        target: &SessionAddress,
        hosted: &mut HashMap<SessionAddress, HostedSession>,
        console: &mut ConsoleState,
        scheduler: &mut SchedulerState,
    ) -> Result<bool, AppError> {
        let Some(runtime) = hosted.remove(target) else {
            return Ok(false);
        };

        let _ = runtime.handle.terminate();
        self.sessions.mark_exited(target);
        self.pty.release(target);
        let active_addresses = self.active_session_addresses();
        console.handle_focus_loss(&active_addresses);
        scheduler.on_manual_switch(console);
        Ok(true)
    }

    fn render_surface(
        &self,
        surface: RenderSurface,
        renderer_state: &mut RendererState,
        renderer: &Renderer,
        console: &ConsoleState,
        scheduler: &SchedulerState,
        command_prompt: &CommandPromptState,
    ) -> Result<(), AppError> {
        match surface {
            RenderSurface::Workspace => self.render_workspace_console(
                renderer_state,
                renderer,
                console,
                scheduler,
                command_prompt,
            ),
            RenderSurface::Server => self.render_host_console(
                renderer_state,
                renderer,
                console,
                scheduler,
                command_prompt,
            ),
        }
    }

    fn render_idle_frame(
        &self,
        surface: &str,
        active_count: usize,
        waiting_count: usize,
        overlay_lines: &[String],
        bottom_line: &str,
    ) -> String {
        let mut lines = workspace_idle_lines(surface, active_count, waiting_count);
        let target_rows = self.terminal.current_size_or_default().rows as usize;
        let reserved_rows = lines.len() + overlay_lines.len() + 1;
        let spacer_rows = target_rows.saturating_sub(reserved_rows);
        lines.extend(std::iter::repeat(String::new()).take(spacer_rows));
        lines.extend(overlay_lines.iter().map(|line| {
            style_overlay_line(line, self.terminal.current_size_or_default().cols as usize)
        }));
        lines.push(style_status_line(
            bottom_line,
            self.terminal.current_size_or_default().cols as usize,
        ));
        lines.join("\r\n")
    }

    fn handle_server(&mut self, command: ServerCommand) -> Result<(), AppError> {
        let runtime = self
            .config
            .runtime_for_server(command.listen.as_deref(), command.node_id.as_deref());
        self.run_local_host(&runtime)
    }
}

struct HostedSession {
    handle: PtyHandle,
    screen_engine: TerminalEngine,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CursorPlacement {
    row: u16,
    col: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RenderSurface {
    Workspace,
    Server,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConsoleAction {
    CreateSession,
    ListSessions,
    CloseCurrentSession,
    DismissOverlay,
    NextSession,
    PreviousSession,
    FocusIndex(usize),
    TogglePeek,
    QuitHost,
}

const COMMAND_BAR_PREFIX: u8 = 0x17;
const COMMAND_BAR_PREFIX_FALLBACK: u8 = 0x07;
const SHORTCUT_PREVIOUS_SESSION: u8 = 0x02;
const SHORTCUT_NEXT_SESSION: u8 = 0x06;
const SHORTCUT_NEW_SESSION: u8 = 0x0e;
const SHORTCUT_LIST_SESSIONS: u8 = 0x0c;
const SHORTCUT_CLOSE_SESSION: u8 = 0x18;
const SHORTCUT_QUIT: u8 = 0x11;

#[derive(Debug, Clone, PartialEq, Eq)]
enum CommandPromptOutcome {
    RenderOnly,
    Execute(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CommandOverlay {
    None,
    Message(String),
    Help,
    Sessions,
}

impl Default for CommandOverlay {
    fn default() -> Self {
        Self::None
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct CommandPromptState {
    open: bool,
    buffer: String,
    overlay: CommandOverlay,
    picker_selection: Option<SessionAddress>,
    pending_picker_escape: Vec<u8>,
    pending_picker_started_at_unix_ms: Option<u128>,
}

impl CommandPromptState {
    fn handle_input(&mut self, bytes: &[u8]) -> Option<CommandPromptOutcome> {
        let mut changed = false;

        for byte in bytes {
            if !self.open {
                if *byte == COMMAND_BAR_PREFIX || *byte == COMMAND_BAR_PREFIX_FALLBACK {
                    self.open = true;
                    self.buffer.clear();
                    self.clear_pending_picker_escape();
                    changed = true;
                }
                continue;
            }

            match *byte {
                COMMAND_BAR_PREFIX | COMMAND_BAR_PREFIX_FALLBACK | 0x1b => {
                    self.open = false;
                    self.buffer.clear();
                    self.clear_pending_picker_escape();
                    changed = true;
                }
                b'\r' | b'\n' => {
                    let command = self.buffer.trim().to_string();
                    self.open = false;
                    self.buffer.clear();
                    self.clear_pending_picker_escape();
                    return Some(CommandPromptOutcome::Execute(command));
                }
                0x08 | 0x7f => {
                    self.buffer.pop();
                    changed = true;
                }
                byte if (0x20..=0x7e).contains(&byte) => {
                    self.buffer.push(byte as char);
                    changed = true;
                }
                _ => {}
            }
        }

        changed.then_some(CommandPromptOutcome::RenderOnly)
    }

    fn toggle_help(&mut self) {
        if matches!(self.overlay, CommandOverlay::Help) {
            self.overlay = CommandOverlay::None;
        } else {
            self.overlay = CommandOverlay::Help;
        }
        self.clear_pending_picker_escape();
    }

    fn toggle_sessions(
        &mut self,
        sessions: &[&crate::session::SessionRecord],
        focused: Option<&SessionAddress>,
    ) {
        if matches!(self.overlay, CommandOverlay::Sessions) {
            self.overlay = CommandOverlay::None;
            self.picker_selection = None;
        } else {
            self.overlay = CommandOverlay::Sessions;
            self.sync_picker_selection(sessions, focused);
        }
        self.clear_pending_picker_escape();
    }

    fn set_message(&mut self, message: impl Into<String>) {
        self.overlay = CommandOverlay::Message(message.into());
        self.clear_pending_picker_escape();
    }

    fn clear_overlay(&mut self) {
        self.overlay = CommandOverlay::None;
        self.picker_selection = None;
        self.clear_pending_picker_escape();
    }

    fn has_overlay(&self) -> bool {
        !matches!(self.overlay, CommandOverlay::None)
    }

    fn dismiss(&mut self) -> bool {
        if self.open {
            self.open = false;
            self.buffer.clear();
            self.clear_pending_picker_escape();
            return true;
        }

        if !matches!(self.overlay, CommandOverlay::None) {
            self.overlay = CommandOverlay::None;
            self.picker_selection = None;
            self.clear_pending_picker_escape();
            return true;
        }

        false
    }

    fn pick_session_index(
        &self,
        bytes: &[u8],
        sessions: &[&crate::session::SessionRecord],
    ) -> Option<usize> {
        if self.open || !matches!(self.overlay, CommandOverlay::Sessions) {
            return None;
        }

        match bytes {
            [digit @ b'1'..=b'9'] => {
                let index = (digit - b'0') as usize;
                let active_len = picker_sessions(sessions).len();
                (index <= active_len).then_some(index)
            }
            _ => None,
        }
    }

    fn submit_overlay(&self, bytes: &[u8]) -> bool {
        if self.open || !self.has_overlay() {
            return false;
        }

        matches!(bytes, b"\r" | b"\n" | b"\r\n")
    }

    fn handle_picker_navigation(
        &mut self,
        bytes: &[u8],
        sessions: &[&crate::session::SessionRecord],
        focused: Option<&SessionAddress>,
        now_unix_ms: u128,
    ) -> Option<PickerNavigationOutcome> {
        if self.open || !matches!(self.overlay, CommandOverlay::Sessions) {
            self.clear_pending_picker_escape();
            return None;
        }

        let mut combined = self.pending_picker_escape.clone();
        combined.extend_from_slice(bytes);

        match combined.as_slice() {
            b"\x1b[A" => {
                self.clear_pending_picker_escape();
                let moved = self.move_picker_previous(sessions, focused);
                Some(if moved {
                    PickerNavigationOutcome::Render
                } else {
                    PickerNavigationOutcome::Consumed
                })
            }
            b"\x1b[B" => {
                self.clear_pending_picker_escape();
                let moved = self.move_picker_next(sessions, focused);
                Some(if moved {
                    PickerNavigationOutcome::Render
                } else {
                    PickerNavigationOutcome::Consumed
                })
            }
            [0x1b] | [0x1b, b'['] => {
                self.pending_picker_escape = combined;
                self.pending_picker_started_at_unix_ms = Some(now_unix_ms);
                Some(PickerNavigationOutcome::Consumed)
            }
            _ => None,
        }
    }

    fn flush_picker_navigation_timeout(&mut self, now_unix_ms: u128) -> bool {
        if self.open || !matches!(self.overlay, CommandOverlay::Sessions) {
            self.clear_pending_picker_escape();
            return false;
        }

        let Some(started_at) = self.pending_picker_started_at_unix_ms else {
            return false;
        };

        if now_unix_ms.saturating_sub(started_at) < PICKER_ESCAPE_TIMEOUT_MS {
            return false;
        }

        let pending = self.pending_picker_escape.clone();
        self.clear_pending_picker_escape();
        if pending == [0x1b] {
            self.clear_overlay();
            true
        } else {
            false
        }
    }

    fn move_picker_previous(
        &mut self,
        sessions: &[&crate::session::SessionRecord],
        focused: Option<&SessionAddress>,
    ) -> bool {
        self.move_picker(sessions, focused, -1)
    }

    fn move_picker_next(
        &mut self,
        sessions: &[&crate::session::SessionRecord],
        focused: Option<&SessionAddress>,
    ) -> bool {
        self.move_picker(sessions, focused, 1)
    }

    fn selected_picker_index(
        &mut self,
        sessions: &[&crate::session::SessionRecord],
        focused: Option<&SessionAddress>,
    ) -> Option<usize> {
        if !matches!(self.overlay, CommandOverlay::Sessions) {
            return None;
        }

        self.sync_picker_selection(sessions, focused);
        let active = picker_sessions(sessions);
        let selected = self.picker_selection.as_ref()?;
        active
            .iter()
            .position(|session| session.address() == selected)
            .map(|index| index + 1)
    }

    fn overlay_lines(
        &self,
        sessions: Vec<&crate::session::SessionRecord>,
        focused: Option<&SessionAddress>,
    ) -> Vec<String> {
        let mut lines = Vec::new();
        let active_sessions = picker_sessions(&sessions);

        match &self.overlay {
            CommandOverlay::None => {}
            CommandOverlay::Message(message) => {
                lines.push(format!("notice: {message}"));
            }
            CommandOverlay::Help => {
                lines.push("help: /new /sessions /focus <n|id> /close /quit /clear".to_string());
                lines.push(
                    "help: Esc hide | Ctrl-B prev | Ctrl-F next | Ctrl-L picker | Ctrl-N new"
                        .to_string(),
                );
            }
            CommandOverlay::Sessions => {
                lines.push(
                    "picker: Up/Down move  ^B prev  ^F next  Enter select  Esc close  1-9 direct"
                        .to_string(),
                );
                let selected = self
                    .picker_selection
                    .as_ref()
                    .or(focused)
                    .map(ToOwned::to_owned);
                for (index, session) in active_sessions.into_iter().take(8).enumerate() {
                    let marker = if Some(session.address()) == selected.as_ref() {
                        ">"
                    } else {
                        " "
                    };
                    lines.push(format!(
                        "{} {:>2}. {} ({})",
                        marker,
                        index + 1,
                        session.address(),
                        session.title
                    ));
                    if let Some(working_dir) = session.current_working_dir.as_deref() {
                        lines.push(format!("   cwd: {working_dir}"));
                    } else {
                        lines.push("   cwd: unknown".to_string());
                    }
                }
            }
        }

        lines.push("keys: ^W cmd  ^B/^F switch  ^N new  ^L picker  ^X close  ^C quit".to_string());

        if self.open {
            lines.push(format!(":{}", self.buffer));
        }

        lines
    }

    fn move_picker(
        &mut self,
        sessions: &[&crate::session::SessionRecord],
        focused: Option<&SessionAddress>,
        delta: isize,
    ) -> bool {
        if self.open || !matches!(self.overlay, CommandOverlay::Sessions) {
            return false;
        }

        let active = picker_sessions(sessions);
        if active.is_empty() {
            return false;
        }

        self.sync_picker_selection(sessions, focused);
        let current = self
            .picker_selection
            .as_ref()
            .and_then(|selected| {
                active
                    .iter()
                    .position(|session| session.address() == selected)
            })
            .unwrap_or(0);
        let len = active.len() as isize;
        let next = ((current as isize + delta).rem_euclid(len)) as usize;
        self.picker_selection = Some(active[next].address().clone());
        true
    }

    fn sync_picker_selection(
        &mut self,
        sessions: &[&crate::session::SessionRecord],
        focused: Option<&SessionAddress>,
    ) {
        let active = picker_sessions(sessions);
        if active.is_empty() {
            self.picker_selection = None;
            return;
        }

        if self
            .picker_selection
            .as_ref()
            .map(|selected| active.iter().any(|session| session.address() == selected))
            .unwrap_or(false)
        {
            return;
        }

        self.picker_selection = focused
            .filter(|target| active.iter().any(|session| session.address() == *target))
            .cloned()
            .or_else(|| active.first().map(|session| session.address().clone()));
    }

    fn clear_pending_picker_escape(&mut self) {
        self.pending_picker_escape.clear();
        self.pending_picker_started_at_unix_ms = None;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PickerNavigationOutcome {
    Consumed,
    Render,
}

fn picker_sessions<'a>(
    sessions: &'a [&'a crate::session::SessionRecord],
) -> Vec<&'a crate::session::SessionRecord> {
    sessions
        .iter()
        .copied()
        .filter(|session| !matches!(session.status, SessionStatus::Exited))
        .collect()
}

#[derive(Debug, Default)]
struct InputTracker {
    pending_bytes: usize,
}

impl InputTracker {
    fn observe(
        &mut self,
        bytes: &[u8],
        console: &mut ConsoleState,
        scheduler: &mut SchedulerState,
        now_unix_ms: u128,
    ) {
        for byte in bytes {
            match *byte {
                b'\r' | b'\n' => {
                    self.pending_bytes = 0;
                    scheduler.on_input_submitted(console, now_unix_ms);
                }
                0x08 | 0x7f => {
                    self.pending_bytes = self.pending_bytes.saturating_sub(1);
                    if self.pending_bytes == 0 {
                        console.clear_input();
                    } else {
                        console.start_typing();
                        console.set_input_len(self.pending_bytes);
                    }
                }
                _ => {
                    self.pending_bytes += 1;
                    console.start_typing();
                    console.set_input_len(self.pending_bytes);
                }
            }
        }
    }
}

enum RuntimeEvent {
    Input(Vec<u8>),
    InputClosed,
    Output {
        session: SessionAddress,
        bytes: Vec<u8>,
    },
    OutputClosed {
        session: SessionAddress,
    },
}

fn workspace_idle_lines(surface: &str, active_count: usize, waiting_count: usize) -> Vec<String> {
    vec![
        format!("WaitAgent | {surface}"),
        format!("active: {active_count} | waiting: {waiting_count}"),
        "hint: Ctrl-W command bar | Ctrl-B/Ctrl-F switch | Ctrl-C quit".to_string(),
    ]
}

fn parse_console_action(bytes: &[u8]) -> Option<ConsoleAction> {
    match bytes {
        [SHORTCUT_INTERRUPT_EXIT] => Some(ConsoleAction::QuitHost),
        [0x1b] => Some(ConsoleAction::DismissOverlay),
        [SHORTCUT_PREVIOUS_SESSION] => Some(ConsoleAction::PreviousSession),
        [SHORTCUT_NEXT_SESSION] => Some(ConsoleAction::NextSession),
        [SHORTCUT_NEW_SESSION] => Some(ConsoleAction::CreateSession),
        [SHORTCUT_LIST_SESSIONS] => Some(ConsoleAction::ListSessions),
        [SHORTCUT_CLOSE_SESSION] => Some(ConsoleAction::CloseCurrentSession),
        [SHORTCUT_QUIT] => Some(ConsoleAction::QuitHost),
        b"\x1bc" => Some(ConsoleAction::CreateSession),
        b"\x1bn" | b"\x1b[1;5I" => Some(ConsoleAction::NextSession),
        b"\x1bp" | b"\x1b[Z" => Some(ConsoleAction::PreviousSession),
        b"\x1bv" => Some(ConsoleAction::TogglePeek),
        b"\x1bx" => Some(ConsoleAction::QuitHost),
        [0x1b, digit @ b'1'..=b'9'] => Some(ConsoleAction::FocusIndex((digit - b'0') as usize)),
        _ => None,
    }
}

fn style_overlay_line(line: &str, width: usize) -> String {
    let padded = pad_line(line, width);
    if line.starts_with(':') {
        format!("{ANSI_BG_COMMAND}{padded}{ANSI_RESET}")
    } else if line.starts_with("keys:") {
        format!("{ANSI_BG_KEYS}{padded}{ANSI_RESET}")
    } else if line.starts_with("sessions:") {
        format!("{ANSI_BG_PICKER}{padded}{ANSI_RESET}")
    } else if line.starts_with("> ") {
        format!("{ANSI_BG_PICKER_ACTIVE}{padded}{ANSI_RESET}")
    } else if line.starts_with("  ") {
        format!("{ANSI_BG_PICKER}{padded}{ANSI_RESET}")
    } else if line.starts_with("notice:") {
        format!("{ANSI_FG_NOTICE}{line}{ANSI_RESET}")
    } else if line.starts_with("help:") {
        format!("{ANSI_FG_ACCENT}{line}{ANSI_RESET}")
    } else {
        line.to_string()
    }
}

fn style_status_line(line: &str, width: usize) -> String {
    format!("{ANSI_BG_BAR}{}{ANSI_RESET}", pad_line(line, width))
}

fn pad_line(line: &str, width: usize) -> String {
    let truncated = line.chars().take(width).collect::<String>();
    let padding = width.saturating_sub(truncated.chars().count());
    format!("{truncated}{}", " ".repeat(padding))
}

fn spawn_stdin_reader(tx: Sender<RuntimeEvent>) {
    thread::spawn(move || {
        let stdin = io::stdin();
        let mut lock = stdin.lock();
        let mut chunk = [0_u8; 1024];

        loop {
            match lock.read(&mut chunk) {
                Ok(0) => {
                    let _ = tx.send(RuntimeEvent::InputClosed);
                    break;
                }
                Ok(count) => {
                    if tx
                        .send(RuntimeEvent::Input(chunk[..count].to_vec()))
                        .is_err()
                    {
                        break;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    let _ = tx.send(RuntimeEvent::InputClosed);
                    break;
                }
            }
        }
    });
}

fn register_client_connection(
    server_runtime: &mut ServerRuntime,
    connection: &mut crate::server::AcceptedConnection,
) -> Result<(), AppError> {
    let hello =
        read_transport_envelope(&mut connection.stream).map_err(ServerRuntimeError::Transport)?;
    if let Some(server_hello) = server_runtime.apply_transport_envelope(
        &connection.connection_id,
        connection.peer_addr,
        hello,
    )? {
        write_transport_envelope(&mut connection.stream, &server_hello)
            .map_err(ServerRuntimeError::Transport)?;
    }

    let heartbeat =
        read_transport_envelope(&mut connection.stream).map_err(ServerRuntimeError::Transport)?;
    let _ = server_runtime.apply_transport_envelope(
        &connection.connection_id,
        connection.peer_addr,
        heartbeat,
    )?;

    Ok(())
}

fn render_command_line(program: &str, args: &[String]) -> String {
    let mut parts = Vec::with_capacity(args.len() + 1);
    parts.push(program.to_string());
    parts.extend(args.iter().cloned());
    parts.join(" ")
}

fn default_shell_program() -> String {
    env::var("SHELL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "/bin/sh".to_string())
}

fn shell_title(program: &str) -> String {
    Path::new(program)
        .file_name()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .unwrap_or(program)
        .to_string()
}

fn spawn_pty_reader(tx_reader: File, tx: Sender<RuntimeEvent>, session: SessionAddress) {
    thread::spawn(move || {
        let mut reader = tx_reader;
        let mut chunk = [0_u8; 4096];

        loop {
            match reader.read(&mut chunk) {
                Ok(0) => {
                    let _ = tx.send(RuntimeEvent::OutputClosed {
                        session: session.clone(),
                    });
                    break;
                }
                Ok(count) => {
                    if tx
                        .send(RuntimeEvent::Output {
                            session: session.clone(),
                            bytes: chunk[..count].to_vec(),
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                Err(error) if error.raw_os_error() == Some(PTY_EOF_ERRNO) => {
                    let _ = tx.send(RuntimeEvent::OutputClosed {
                        session: session.clone(),
                    });
                    break;
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    let _ = tx.send(RuntimeEvent::OutputClosed {
                        session: session.clone(),
                    });
                    break;
                }
            }
        }
    });
}

fn now_unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn format_exit_status(status: ExitStatus) -> String {
    if status.success() {
        "success".to_string()
    } else {
        status.to_string()
    }
}

fn print_banner() {
    println!(
        r#" __        __    _ _      _                            _
 \ \      / /_ _(_) |_   / \   __ _  ___ _ __   __ _ | |_
  \ \ /\ / / _` | | __| / _ \ / _` |/ _ \ '_ \ / _` || __|
   \ V  V / (_| | | |_ / ___ \ (_| |  __/ | | | (_| || |_
    \_/\_/ \__,_|_|\__/_/   \_\__, |\___|_| |_|\__,_| \__|
                              |___/
"#
    );
    println!("One terminal. Many agents. Zero tab thrash.");
    println!();
}

fn print_runtime_header(command: &str, config: &AppConfig, session: Option<&SessionAddress>) {
    println!("waitagent_command: {command}");
    println!("node_id: {}", config.node.node_id);
    println!("mode: {}", config.mode_name());
    println!("listen_addr: {}", config.network.listen_addr);
    println!("access_point: {}", config.network.access_point_display());

    if let Some(address) = session {
        println!("session: {address}");
    }
}

#[derive(Debug)]
pub enum AppError {
    Cli(crate::cli::CliError),
    Client(ClientRuntimeError),
    InvalidCommand(String),
    Pty(crate::pty::PtyError),
    Render(RenderError),
    Server(ServerRuntimeError),
    Terminal(crate::terminal::TerminalError),
    Io(String, io::Error),
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cli(error) => write!(f, "{error}"),
            Self::Client(error) => write!(f, "{error}"),
            Self::InvalidCommand(message) => write!(f, "invalid command: {message}"),
            Self::Pty(error) => write!(f, "{error}"),
            Self::Render(error) => write!(f, "{error}"),
            Self::Server(error) => write!(f, "{error}"),
            Self::Terminal(error) => write!(f, "{error}"),
            Self::Io(context, error) => write!(f, "{context}: {error}"),
        }
    }
}

impl Error for AppError {}

impl From<crate::cli::CliError> for AppError {
    fn from(value: crate::cli::CliError) -> Self {
        Self::Cli(value)
    }
}

impl From<crate::pty::PtyError> for AppError {
    fn from(value: crate::pty::PtyError) -> Self {
        Self::Pty(value)
    }
}

impl From<ClientRuntimeError> for AppError {
    fn from(value: ClientRuntimeError) -> Self {
        Self::Client(value)
    }
}

impl From<crate::renderer::RenderError> for AppError {
    fn from(value: crate::renderer::RenderError) -> Self {
        Self::Render(value)
    }
}

impl From<crate::server::ServerRuntimeError> for AppError {
    fn from(value: crate::server::ServerRuntimeError) -> Self {
        Self::Server(value)
    }
}

impl From<crate::terminal::TerminalError> for AppError {
    fn from(value: crate::terminal::TerminalError) -> Self {
        Self::Terminal(value)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        default_shell_program, parse_console_action, shell_title, CommandOverlay,
        CommandPromptState, ConsoleAction, InputTracker, PICKER_ESCAPE_TIMEOUT_MS,
        SHORTCUT_NEXT_SESSION, SHORTCUT_PREVIOUS_SESSION,
    };
    use crate::client::normalize_endpoint;
    use crate::console::ConsoleState;
    use crate::scheduler::{SchedulerPhase, SchedulerState};
    use crate::session::{SessionAddress, SessionRegistry};

    #[test]
    fn input_tracker_enters_typing_and_submitted_states() {
        let mut tracker = InputTracker::default();
        let mut console = ConsoleState::new("console-1");
        console.focus(SessionAddress::new("local", "session-1"));
        let mut scheduler = SchedulerState::new();

        tracker.observe(b"abc", &mut console, &mut scheduler, 100);
        assert!(!console.can_switch());

        tracker.observe(b"\r", &mut console, &mut scheduler, 200);
        assert!(console.can_switch());
        assert_eq!(
            scheduler.phase(),
            &SchedulerPhase::ObservingContinuation {
                session: console.focused_session.clone(),
                entered_at_unix_ms: 200,
                saw_output: false,
            }
        );
    }

    #[test]
    fn input_tracker_clears_typing_after_backspacing_all_bytes() {
        let mut tracker = InputTracker::default();
        let mut console = ConsoleState::new("console-1");
        console.focus(SessionAddress::new("local", "session-1"));
        let mut scheduler = SchedulerState::new();

        tracker.observe(b"ab", &mut console, &mut scheduler, 100);
        assert!(!console.can_switch());

        tracker.observe(&[0x7f, 0x7f], &mut console, &mut scheduler, 150);
        assert!(console.can_switch());
    }

    #[test]
    fn console_action_parser_recognizes_focus_shortcuts() {
        assert_eq!(
            parse_console_action(b"\x1bc"),
            Some(ConsoleAction::CreateSession)
        );
        assert_eq!(
            parse_console_action(&[0x1b]),
            Some(ConsoleAction::DismissOverlay)
        );
        assert_eq!(
            parse_console_action(&[SHORTCUT_NEXT_SESSION]),
            Some(ConsoleAction::NextSession)
        );
        assert_eq!(
            parse_console_action(&[SHORTCUT_PREVIOUS_SESSION]),
            Some(ConsoleAction::PreviousSession)
        );
        assert_eq!(
            parse_console_action(b"\x1bn"),
            Some(ConsoleAction::NextSession)
        );
        assert_eq!(
            parse_console_action(b"\x1bp"),
            Some(ConsoleAction::PreviousSession)
        );
        assert_eq!(
            parse_console_action(b"\x1b3"),
            Some(ConsoleAction::FocusIndex(3))
        );
        assert_eq!(
            parse_console_action(b"\x1bv"),
            Some(ConsoleAction::TogglePeek)
        );
        assert_eq!(
            parse_console_action(b"\x1bx"),
            Some(ConsoleAction::QuitHost)
        );
        assert_eq!(parse_console_action(b"plain input"), None);
    }

    #[test]
    fn normalizes_control_address_schemes() {
        assert_eq!(normalize_endpoint("ws://127.0.0.1:7474"), "127.0.0.1:7474");
        assert_eq!(normalize_endpoint("tcp://127.0.0.1:7474"), "127.0.0.1:7474");
        assert_eq!(normalize_endpoint("127.0.0.1:7474"), "127.0.0.1:7474");
    }

    #[test]
    fn derives_shell_title_from_program_path() {
        assert_eq!(shell_title("/bin/bash"), "bash");
        assert_eq!(shell_title("zsh"), "zsh");
    }

    #[test]
    fn falls_back_to_posix_shell_when_shell_env_is_missing() {
        let original = std::env::var_os("SHELL");
        std::env::remove_var("SHELL");

        let shell = default_shell_program();

        match original {
            Some(value) => std::env::set_var("SHELL", value),
            None => std::env::remove_var("SHELL"),
        }

        assert_eq!(shell, "/bin/sh");
    }

    #[test]
    fn picker_uses_split_arrow_sequences_for_navigation() {
        let mut registry = SessionRegistry::new();
        let first = registry.create_local_session(
            "local".to_string(),
            "bash".to_string(),
            "bash".to_string(),
        );
        let second = registry.create_local_session(
            "local".to_string(),
            "zsh".to_string(),
            "zsh".to_string(),
        );
        let sessions = registry.list();
        let focused = Some(first.address());
        let mut prompt = CommandPromptState::default();
        prompt.toggle_sessions(&sessions, focused);

        assert!(prompt
            .handle_picker_navigation(&[0x1b], &sessions, focused, 100)
            .is_some());
        assert!(prompt
            .handle_picker_navigation(b"[", &sessions, focused, 110)
            .is_some());
        assert!(prompt
            .handle_picker_navigation(b"B", &sessions, focused, 120)
            .is_some());

        assert_eq!(prompt.overlay, CommandOverlay::Sessions);
        assert_eq!(
            prompt.selected_picker_index(&sessions, Some(second.address())),
            Some(2)
        );
    }

    #[test]
    fn picker_closes_after_escape_timeout() {
        let mut registry = SessionRegistry::new();
        let first = registry.create_local_session(
            "local".to_string(),
            "bash".to_string(),
            "bash".to_string(),
        );
        let sessions = registry.list();
        let focused = Some(first.address());
        let mut prompt = CommandPromptState::default();
        prompt.toggle_sessions(&sessions, focused);

        assert!(prompt
            .handle_picker_navigation(&[0x1b], &sessions, focused, 100)
            .is_some());
        assert!(prompt.flush_picker_navigation_timeout(100 + PICKER_ESCAPE_TIMEOUT_MS + 1));
        assert_eq!(prompt.overlay, CommandOverlay::None);
    }
}
