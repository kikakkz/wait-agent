use crate::agent::live_agent_label as live_command_label;
use crate::cli::{
    AttachCommand, Cli, Command, DaemonCommand, DetachCommand, ListCommand, RunCommand,
    ServerCommand, StatusCommand, WorkspaceCommand,
};
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
use crate::terminal::{AlternateScreenGuard, TerminalEngine, TerminalRuntime};
use crate::transcript::TerminalTranscript;
use crate::transport::{read_transport_envelope, write_transport_envelope};
use std::collections::{HashMap, HashSet, VecDeque};
use std::env;
use std::error::Error;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const EVENT_LOOP_TICK: Duration = Duration::from_millis(50);
const PICKER_ESCAPE_TIMEOUT_MS: u128 = 150;
const RESET_FRAME_CURSOR: &str = "\x1b[H";
const RESTORE_SCREEN: &str = "\x1b[2J\x1b[H\x1b[?25h";
const CLEAR_SCROLLBACK_AND_SCREEN: &str = "\x1b[3J\x1b[2J\x1b[H";
const SHORTCUT_INTERRUPT_EXIT: u8 = 0x03;
const SHORTCUT_NATIVE_FULLSCREEN: u8 = 0x0f;
const ANSI_RESET: &str = "\x1b[0m";
const ANSI_SYNC_UPDATE_START: &str = "\x1b[?2026h";
const ANSI_SYNC_UPDATE_END: &str = "\x1b[?2026l";
const ANSI_FG_ACCENT: &str = "\x1b[38;5;81m";
const ANSI_FG_NOTICE: &str = "\x1b[38;5;120m";
const ANSI_BG_BAR: &str = "\x1b[48;5;24m\x1b[38;5;255m";
const ANSI_FG_FOOTER_DIVIDER: &str = "\x1b[1;38;5;255m";
const ANSI_BG_KEYS: &str = "\x1b[48;5;236m\x1b[38;5;252m";
const ANSI_BG_COMMAND: &str = "\x1b[48;5;238m\x1b[38;5;255m";
const ANSI_BG_PICKER: &str = "\x1b[48;5;235m\x1b[38;5;250m";
const ANSI_BG_PICKER_ACTIVE: &str = "\x1b[48;5;31m\x1b[38;5;255m";
const ANSI_BG_SIDEBAR_HEADER: &str = "\x1b[48;5;236m\x1b[1;38;5;255m";
const ANSI_BG_SIDEBAR_HINT: &str = "\x1b[48;5;235m\x1b[38;5;246m";
const ANSI_BG_SIDEBAR_ITEM: &str = "\x1b[48;5;234m\x1b[38;5;250m";
const ANSI_BG_SIDEBAR_ACTIVE: &str = "\x1b[48;5;240m\x1b[1;38;5;255m";
const ANSI_BG_SIDEBAR_DETAIL: &str = "\x1b[48;5;236m\x1b[38;5;252m";
const ANSI_FG_SIDEBAR_RUNNING: &str = "\x1b[38;5;121m";
const ANSI_FG_SIDEBAR_INPUT: &str = "\x1b[38;5;227m";
const ANSI_FG_SIDEBAR_CONFIRM: &str = "\x1b[38;5;215m";
const LIVE_SURFACE_STATUS_ROWS: u16 = 3;
const MANAGED_CONSOLE_RESERVED_ROWS: u16 = LIVE_SURFACE_STATUS_ROWS;
const SIDEBAR_NAVIGATION_TIMEOUT_MS: u128 = 150;
const COLLAPSED_SIDEBAR_WIDTH: usize = 2;
const STARTUP_SHELL_WARMUP: Duration = Duration::from_millis(500);
const SIDEBAR_STARTUP_FULL_REDRAWS: u8 = 2;

pub fn run() -> Result<(), AppError> {
    let cli = Cli::parse(std::env::args_os())?;
    let config = AppConfig::from_env();

    match cli.command {
        Command::Workspace(workspace) => {
            return crate::lifecycle::run_workspace_entry(config, workspace)
                .map_err(AppError::from);
        }
        Command::Daemon(command) => {
            return crate::lifecycle::run_daemon(config, command).map_err(AppError::from);
        }
        Command::Attach(command) => {
            return crate::lifecycle::run_attach(command).map_err(AppError::from);
        }
        Command::List(command) => {
            return crate::lifecycle::run_list(command).map_err(AppError::from);
        }
        Command::Status(command) => {
            return crate::lifecycle::run_status(command).map_err(AppError::from);
        }
        Command::Detach(command) => {
            return crate::lifecycle::run_detach(command).map_err(AppError::from);
        }
        Command::WorkspaceInternal(_) | Command::Run(_) | Command::Server(_) | Command::Help(_) => {
        }
    }

    let mut app = App::new(config);
    app.execute(cli.command)
}

struct App {
    config: AppConfig,
    sessions: SessionRegistry,
    pty: PtyManager,
    terminal: TerminalRuntime,
    input_trace: Option<InputTrace>,
    output_trace: Option<OutputTrace>,
}

struct InputTrace {
    file: File,
}

struct OutputTrace {
    file: File,
}

impl InputTrace {
    fn from_path(path: Option<&str>) -> Option<Self> {
        let path = path?.trim();
        if path.is_empty() {
            return None;
        }

        match OpenOptions::new().create(true).append(true).open(path) {
            Ok(file) => Some(Self { file }),
            Err(error) => {
                eprintln!("waitagent: failed to open input trace file `{path}`: {error}");
                None
            }
        }
    }

    fn record(
        &mut self,
        surface: &str,
        bytes: &[u8],
        command_prompt: &CommandPromptState,
        native_fullscreen_active: bool,
        passthrough_display: bool,
    ) {
        let hex = bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<Vec<_>>()
            .join(" ");
        let escaped = escape_input_bytes(bytes);
        let shortcuts = describe_shortcut_matches(bytes);
        let _ = writeln!(
            self.file,
            "t={} surface={} bytes=[{}] text=\"{}\" shortcuts={} prompt_open={} native_fullscreen={} passthrough={}",
            now_unix_ms(),
            surface,
            hex,
            escaped,
            shortcuts,
            command_prompt.open,
            native_fullscreen_active,
            passthrough_display,
        );
        let _ = self.file.flush();
    }
}

impl OutputTrace {
    fn from_path(path: Option<&str>) -> Option<Self> {
        let path = path?.trim();
        if path.is_empty() {
            return None;
        }

        match OpenOptions::new().create(true).append(true).open(path) {
            Ok(file) => Some(Self { file }),
            Err(error) => {
                eprintln!("waitagent: failed to open output trace file `{path}`: {error}");
                None
            }
        }
    }

    fn record(&mut self, surface: &str, session: &SessionAddress, bytes: &[u8]) {
        let hex = bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<Vec<_>>()
            .join(" ");
        let escaped = escape_input_bytes(bytes);
        let _ = writeln!(
            self.file,
            "t={} surface={} session={} bytes=[{}] text=\"{}\"",
            now_unix_ms(),
            surface,
            session,
            hex,
            escaped,
        );
        let _ = self.file.flush();
    }
}

impl App {
    fn new(config: AppConfig) -> Self {
        Self {
            input_trace: InputTrace::from_path(config.debug.input_trace_path.as_deref()),
            output_trace: OutputTrace::from_path(config.debug.output_trace_path.as_deref()),
            config,
            sessions: SessionRegistry::new(),
            pty: PtyManager::new(),
            terminal: TerminalRuntime::stdio(),
        }
    }

    fn trace_input(
        &mut self,
        surface: &str,
        bytes: &[u8],
        command_prompt: &CommandPromptState,
        native_fullscreen_active: bool,
        passthrough_display: bool,
    ) {
        if let Some(trace) = self.input_trace.as_mut() {
            trace.record(
                surface,
                bytes,
                command_prompt,
                native_fullscreen_active,
                passthrough_display,
            );
        }
    }

    fn trace_output(&mut self, surface: &str, session: &SessionAddress, bytes: &[u8]) {
        if let Some(trace) = self.output_trace.as_mut() {
            trace.record(surface, session, bytes);
        }
    }

    fn execute(&mut self, command: Command) -> Result<(), AppError> {
        match command {
            Command::Workspace(workspace) | Command::WorkspaceInternal(workspace) => {
                self.handle_workspace(workspace)
            }
            Command::Daemon(command) => self.handle_unexpected_daemon(command),
            Command::Attach(command) => self.handle_unexpected_attach(command),
            Command::List(command) => self.handle_unexpected_list(command),
            Command::Status(command) => self.handle_unexpected_status(command),
            Command::Detach(command) => self.handle_unexpected_detach(command),
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

    fn handle_unexpected_daemon(&mut self, _command: DaemonCommand) -> Result<(), AppError> {
        Err(AppError::InvalidCommand(
            "daemon should be handled before workspace app startup".to_string(),
        ))
    }

    fn handle_unexpected_attach(&mut self, _command: AttachCommand) -> Result<(), AppError> {
        Err(AppError::InvalidCommand(
            "attach should be handled before workspace app startup".to_string(),
        ))
    }

    fn handle_unexpected_list(&mut self, _command: ListCommand) -> Result<(), AppError> {
        Err(AppError::InvalidCommand(
            "ls should be handled before workspace app startup".to_string(),
        ))
    }

    fn handle_unexpected_status(&mut self, _command: StatusCommand) -> Result<(), AppError> {
        Err(AppError::InvalidCommand(
            "status should be handled before workspace app startup".to_string(),
        ))
    }

    fn handle_unexpected_detach(&mut self, _command: DetachCommand) -> Result<(), AppError> {
        Err(AppError::InvalidCommand(
            "detach should be handled before workspace app startup".to_string(),
        ))
    }

    fn run_local_workspace(&mut self, runtime: &AppConfig) -> Result<(), AppError> {
        let terminal_snapshot = self.terminal.snapshot()?;
        if !terminal_snapshot.input_is_tty || !terminal_snapshot.output_is_tty {
            return Err(AppError::Terminal(crate::terminal::TerminalError::NotTty(
                "workspace console".to_string(),
            )));
        }

        let mut alternate_screen = self.terminal.enter_alternate_screen()?;
        let _raw_mode = self.terminal.enter_raw_mode()?;
        let mut console = ConsoleState::new("workspace-console");
        let mut scheduler = SchedulerState::new();
        let renderer = Renderer::new();
        let mut renderer_state = RendererState::default();
        let mut input_tracker = InputTracker::default();
        let mut command_prompt = CommandPromptState::default();
        let mut sidebar = SidebarState::default();
        let mut live_surface = LiveSurfaceState::default();
        let mut native_fullscreen = NativeFullscreenState::default();
        let mut hosted = HashMap::<SessionAddress, HostedSession>::new();

        let (tx, rx) = mpsc::channel();
        spawn_stdin_reader(tx.clone());

        let initial_session = self.spawn_default_shell_session(
            &runtime.node.node_id,
            terminal_snapshot.size,
            false,
            &mut hosted,
            &tx,
        )?;
        console.focus(initial_session);
        sidebar.force_full_redraws = SIDEBAR_STARTUP_FULL_REDRAWS;
        self.warm_up_shell_session(&rx, &mut hosted, STARTUP_SHELL_WARMUP)?;

        self.refresh_surface(
            RenderSurface::Workspace,
            &mut live_surface,
            &mut hosted,
            &mut renderer_state,
            &renderer,
            &console,
            &scheduler,
            &command_prompt,
            &mut sidebar,
        )?;
        let mut last_waiting_count = scheduler.waiting_queue().entries().len();
        let mut last_waiting_addresses = scheduler.waiting_queue().addresses();
        let mut should_exit = false;
        let mut pending_events = VecDeque::new();

        while !should_exit {
            let mut suppress_scheduler_refresh = false;
            match next_runtime_event(&rx, &mut pending_events) {
                Ok(RuntimeEvent::Input(bytes)) => {
                    let input_received_at = now_unix_ms();
                    let passthrough_display =
                        self.focused_session_owns_passthrough_display(&live_surface, &console);
                    self.trace_input(
                        "workspace",
                        &bytes,
                        &command_prompt,
                        native_fullscreen.is_active(),
                        passthrough_display,
                    );
                    if native_fullscreen.is_active() {
                        let fullscreen_input = native_fullscreen.handle_input(&bytes);
                        if let Some(target) = native_fullscreen.session().cloned() {
                            self.forward_native_fullscreen_input(
                                &target,
                                &fullscreen_input.forwarded,
                                &mut hosted,
                                &mut console,
                                &mut scheduler,
                                &mut input_tracker,
                                &mut command_prompt,
                                &mut live_surface,
                            )?;
                        }
                        if fullscreen_input.exit_requested {
                            self.exit_native_fullscreen(
                                RenderSurface::Workspace,
                                &mut native_fullscreen,
                                &mut alternate_screen,
                                &mut live_surface,
                                &mut hosted,
                                &mut renderer_state,
                                &renderer,
                                &console,
                                &scheduler,
                                &command_prompt,
                                &mut sidebar,
                            )?;
                        }
                        continue;
                    }
                    if shortcut_matches(bytes.as_slice(), SHORTCUT_FULLSCREEN) {
                        if self.enter_native_fullscreen(
                            &mut native_fullscreen,
                            &mut alternate_screen,
                            &mut live_surface,
                            &mut hosted,
                            &console,
                        )? {
                            command_prompt.clear_overlay();
                        }
                        continue;
                    }
                    if shortcut_matches(bytes.as_slice(), COMMAND_BAR_PREFIX)
                        || shortcut_matches(bytes.as_slice(), COMMAND_BAR_PREFIX_FALLBACK)
                    {
                        if let Some(outcome) = command_prompt.handle_input(&bytes) {
                            should_exit = self.apply_command_outcome(
                                outcome,
                                runtime,
                                terminal_snapshot.size,
                                &mut native_fullscreen,
                                &mut alternate_screen,
                                &mut live_surface,
                                &mut hosted,
                                &tx,
                                &mut console,
                                &mut scheduler,
                                &mut renderer_state,
                                &renderer,
                                &mut command_prompt,
                                &mut sidebar,
                                RenderSurface::Workspace,
                            )?;
                        }
                        continue;
                    }
                    let allow_interrupt_exit =
                        !self.focused_session_owns_passthrough_display(&live_surface, &console);
                    if let Some(outcome) = command_prompt.handle_picker_navigation(
                        &bytes,
                        &self.sessions.list(),
                        console.focused_session.as_ref(),
                        input_received_at,
                    ) {
                        match outcome {
                            PickerNavigationOutcome::Consumed => {}
                            PickerNavigationOutcome::Render => {
                                self.refresh_surface(
                                    RenderSurface::Workspace,
                                    &mut live_surface,
                                    &mut hosted,
                                    &mut renderer_state,
                                    &renderer,
                                    &console,
                                    &scheduler,
                                    &command_prompt,
                                    &mut sidebar,
                                )?;
                            }
                            PickerNavigationOutcome::Submit => {
                                if let Some(index) = command_prompt.selected_picker_index(
                                    &self.sessions.list(),
                                    console.focused_session.as_ref(),
                                ) {
                                    should_exit = self.apply_workspace_action(
                                        ConsoleAction::FocusIndex(index),
                                        runtime,
                                        terminal_snapshot.size,
                                        &mut live_surface,
                                        &mut hosted,
                                        &tx,
                                        &mut console,
                                        &mut scheduler,
                                        &mut renderer_state,
                                        &renderer,
                                        &mut command_prompt,
                                        &mut sidebar,
                                    )?;
                                } else {
                                    should_exit = self.apply_workspace_action(
                                        ConsoleAction::DismissOverlay,
                                        runtime,
                                        terminal_snapshot.size,
                                        &mut live_surface,
                                        &mut hosted,
                                        &tx,
                                        &mut console,
                                        &mut scheduler,
                                        &mut renderer_state,
                                        &renderer,
                                        &mut command_prompt,
                                        &mut sidebar,
                                    )?;
                                }
                            }
                        }
                    } else if matches!(
                        parse_console_action(
                            &bytes,
                            command_prompt.wants_escape_dismiss(),
                            allow_interrupt_exit,
                        ),
                        Some(ConsoleAction::QuitHost)
                    ) {
                        should_exit = true;
                    } else if let Some(outcome) = command_prompt.handle_input(&bytes) {
                        should_exit = self.apply_command_outcome(
                            outcome,
                            runtime,
                            terminal_snapshot.size,
                            &mut native_fullscreen,
                            &mut alternate_screen,
                            &mut live_surface,
                            &mut hosted,
                            &tx,
                            &mut console,
                            &mut scheduler,
                            &mut renderer_state,
                            &renderer,
                            &mut command_prompt,
                            &mut sidebar,
                            RenderSurface::Workspace,
                        )?;
                    } else if let Some(outcome) = {
                        let previous_sidebar = sidebar.clone();
                        sidebar
                            .handle_navigation(
                                &bytes,
                                &self.sessions.list(),
                                console.focused_session.as_ref(),
                                console.can_switch(),
                                &command_prompt,
                                input_received_at,
                            )
                            .map(|outcome| (previous_sidebar, outcome))
                    } {
                        let (previous_sidebar, outcome) = outcome;
                        match outcome {
                            SidebarNavigationOutcome::Consumed => {}
                            SidebarNavigationOutcome::Render => {
                                self.refresh_after_sidebar_navigation(
                                    RenderSurface::Workspace,
                                    &previous_sidebar,
                                    &mut live_surface,
                                    &mut hosted,
                                    &mut renderer_state,
                                    &renderer,
                                    &console,
                                    &scheduler,
                                    &command_prompt,
                                    &mut sidebar,
                                )?;
                            }
                            SidebarNavigationOutcome::Submit(address) => {
                                should_exit = self.apply_workspace_action(
                                    ConsoleAction::FocusAddress(address),
                                    runtime,
                                    terminal_snapshot.size,
                                    &mut live_surface,
                                    &mut hosted,
                                    &tx,
                                    &mut console,
                                    &mut scheduler,
                                    &mut renderer_state,
                                    &renderer,
                                    &mut command_prompt,
                                    &mut sidebar,
                                )?;
                            }
                        }
                    } else if let Some(action) = parse_console_action(
                        &bytes,
                        command_prompt.wants_escape_dismiss(),
                        allow_interrupt_exit,
                    ) {
                        match action {
                            ConsoleAction::PreviousSession
                                if command_prompt.move_picker_previous(
                                    &self.sessions.list(),
                                    console.focused_session.as_ref(),
                                ) =>
                            {
                                self.refresh_surface(
                                    RenderSurface::Workspace,
                                    &mut live_surface,
                                    &mut hosted,
                                    &mut renderer_state,
                                    &renderer,
                                    &console,
                                    &scheduler,
                                    &command_prompt,
                                    &mut sidebar,
                                )?;
                            }
                            ConsoleAction::NextSession
                                if command_prompt.move_picker_next(
                                    &self.sessions.list(),
                                    console.focused_session.as_ref(),
                                ) =>
                            {
                                self.refresh_surface(
                                    RenderSurface::Workspace,
                                    &mut live_surface,
                                    &mut hosted,
                                    &mut renderer_state,
                                    &renderer,
                                    &console,
                                    &scheduler,
                                    &command_prompt,
                                    &mut sidebar,
                                )?;
                            }
                            ConsoleAction::EnterNativeFullscreen => {
                                if self.enter_native_fullscreen(
                                    &mut native_fullscreen,
                                    &mut alternate_screen,
                                    &mut live_surface,
                                    &mut hosted,
                                    &console,
                                )? {
                                    command_prompt.clear_overlay();
                                }
                            }
                            _ => {
                                should_exit = self.apply_workspace_action(
                                    action,
                                    runtime,
                                    terminal_snapshot.size,
                                    &mut live_surface,
                                    &mut hosted,
                                    &tx,
                                    &mut console,
                                    &mut scheduler,
                                    &mut renderer_state,
                                    &renderer,
                                    &mut command_prompt,
                                    &mut sidebar,
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
                            &mut live_surface,
                            &mut hosted,
                            &tx,
                            &mut console,
                            &mut scheduler,
                            &mut renderer_state,
                            &renderer,
                            &mut command_prompt,
                            &mut sidebar,
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
                                &mut live_surface,
                                &mut hosted,
                                &tx,
                                &mut console,
                                &mut scheduler,
                                &mut renderer_state,
                                &renderer,
                                &mut command_prompt,
                                &mut sidebar,
                            )?;
                        } else {
                            should_exit = self.apply_workspace_action(
                                ConsoleAction::DismissOverlay,
                                runtime,
                                terminal_snapshot.size,
                                &mut live_surface,
                                &mut hosted,
                                &tx,
                                &mut console,
                                &mut scheduler,
                                &mut renderer_state,
                                &renderer,
                                &mut command_prompt,
                                &mut sidebar,
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
                                match outcome {
                                    PickerNavigationOutcome::Consumed => {}
                                    PickerNavigationOutcome::Render => {
                                        self.refresh_surface(
                                            RenderSurface::Workspace,
                                            &mut live_surface,
                                            &mut hosted,
                                            &mut renderer_state,
                                            &renderer,
                                            &console,
                                            &scheduler,
                                            &command_prompt,
                                            &mut sidebar,
                                        )?;
                                    }
                                    PickerNavigationOutcome::Submit => {
                                        if let Some(index) = command_prompt.selected_picker_index(
                                            &self.sessions.list(),
                                            console.focused_session.as_ref(),
                                        ) {
                                            should_exit = self.apply_workspace_action(
                                                ConsoleAction::FocusIndex(index),
                                                runtime,
                                                terminal_snapshot.size,
                                                &mut live_surface,
                                                &mut hosted,
                                                &tx,
                                                &mut console,
                                                &mut scheduler,
                                                &mut renderer_state,
                                                &renderer,
                                                &mut command_prompt,
                                                &mut sidebar,
                                            )?;
                                        } else {
                                            should_exit = self.apply_workspace_action(
                                                ConsoleAction::DismissOverlay,
                                                runtime,
                                                terminal_snapshot.size,
                                                &mut live_surface,
                                                &mut hosted,
                                                &tx,
                                                &mut console,
                                                &mut scheduler,
                                                &mut renderer_state,
                                                &renderer,
                                                &mut command_prompt,
                                                &mut sidebar,
                                            )?;
                                        }
                                    }
                                }
                            } else if let Some(outcome) = command_prompt.handle_input(&single) {
                                handled_control = true;
                                should_exit = self.apply_command_outcome(
                                    outcome,
                                    runtime,
                                    terminal_snapshot.size,
                                    &mut native_fullscreen,
                                    &mut alternate_screen,
                                    &mut live_surface,
                                    &mut hosted,
                                    &tx,
                                    &mut console,
                                    &mut scheduler,
                                    &mut renderer_state,
                                    &renderer,
                                    &mut command_prompt,
                                    &mut sidebar,
                                    RenderSurface::Workspace,
                                )?;
                            } else if let Some((previous_sidebar, outcome)) = {
                                let previous_sidebar = sidebar.clone();
                                sidebar
                                    .handle_navigation(
                                        &single,
                                        &self.sessions.list(),
                                        console.focused_session.as_ref(),
                                        console.can_switch(),
                                        &command_prompt,
                                        now_unix_ms(),
                                    )
                                    .map(|outcome| (previous_sidebar, outcome))
                            } {
                                handled_control = true;
                                match outcome {
                                    SidebarNavigationOutcome::Consumed => {}
                                    SidebarNavigationOutcome::Render => {
                                        self.refresh_after_sidebar_navigation(
                                            RenderSurface::Workspace,
                                            &previous_sidebar,
                                            &mut live_surface,
                                            &mut hosted,
                                            &mut renderer_state,
                                            &renderer,
                                            &console,
                                            &scheduler,
                                            &command_prompt,
                                            &mut sidebar,
                                        )?;
                                    }
                                    SidebarNavigationOutcome::Submit(address) => {
                                        should_exit = self.apply_workspace_action(
                                            ConsoleAction::FocusAddress(address),
                                            runtime,
                                            terminal_snapshot.size,
                                            &mut live_surface,
                                            &mut hosted,
                                            &tx,
                                            &mut console,
                                            &mut scheduler,
                                            &mut renderer_state,
                                            &renderer,
                                            &mut command_prompt,
                                            &mut sidebar,
                                        )?;
                                    }
                                }
                            } else if let Some(action) = parse_console_action(
                                &single,
                                command_prompt.wants_escape_dismiss(),
                                allow_interrupt_exit,
                            ) {
                                handled_control = true;
                                match action {
                                    ConsoleAction::PreviousSession
                                        if command_prompt.move_picker_previous(
                                            &self.sessions.list(),
                                            console.focused_session.as_ref(),
                                        ) =>
                                    {
                                        self.refresh_surface(
                                            RenderSurface::Workspace,
                                            &mut live_surface,
                                            &mut hosted,
                                            &mut renderer_state,
                                            &renderer,
                                            &console,
                                            &scheduler,
                                            &command_prompt,
                                            &mut sidebar,
                                        )?;
                                    }
                                    ConsoleAction::NextSession
                                        if command_prompt.move_picker_next(
                                            &self.sessions.list(),
                                            console.focused_session.as_ref(),
                                        ) =>
                                    {
                                        self.refresh_surface(
                                            RenderSurface::Workspace,
                                            &mut live_surface,
                                            &mut hosted,
                                            &mut renderer_state,
                                            &renderer,
                                            &console,
                                            &scheduler,
                                            &command_prompt,
                                            &mut sidebar,
                                        )?;
                                    }
                                    ConsoleAction::EnterNativeFullscreen => {
                                        handled_control = true;
                                        if self.enter_native_fullscreen(
                                            &mut native_fullscreen,
                                            &mut alternate_screen,
                                            &mut live_surface,
                                            &mut hosted,
                                            &console,
                                        )? {
                                            command_prompt.clear_overlay();
                                        }
                                    }
                                    _ => {
                                        should_exit = self.apply_workspace_action(
                                            action,
                                            runtime,
                                            terminal_snapshot.size,
                                            &mut live_surface,
                                            &mut hosted,
                                            &tx,
                                            &mut console,
                                            &mut scheduler,
                                            &mut renderer_state,
                                            &renderer,
                                            &mut command_prompt,
                                            &mut sidebar,
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
                                    &mut live_surface,
                                    &mut hosted,
                                    &tx,
                                    &mut console,
                                    &mut scheduler,
                                    &mut renderer_state,
                                    &renderer,
                                    &mut command_prompt,
                                    &mut sidebar,
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
                                        &mut live_surface,
                                        &mut hosted,
                                        &tx,
                                        &mut console,
                                        &mut scheduler,
                                        &mut renderer_state,
                                        &renderer,
                                        &mut command_prompt,
                                        &mut sidebar,
                                    )?;
                                } else {
                                    should_exit = self.apply_workspace_action(
                                        ConsoleAction::DismissOverlay,
                                        runtime,
                                        terminal_snapshot.size,
                                        &mut live_surface,
                                        &mut hosted,
                                        &tx,
                                        &mut console,
                                        &mut scheduler,
                                        &mut renderer_state,
                                        &renderer,
                                        &mut command_prompt,
                                        &mut sidebar,
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
                            let bytes_to_forward = if sidebar.modal_active(&command_prompt) {
                                Vec::new()
                            } else if handled_control {
                                residual
                            } else {
                                bytes
                            };
                            let mut refresh_after_live_command = false;
                            let previous_sidebar = sidebar.clone();
                            if let Some(target) = console.input_owner_session().cloned() {
                                if !bytes_to_forward.is_empty() {
                                    command_prompt
                                        .clear_message_on_forwarded_input(&bytes_to_forward);
                                    input_tracker.observe(
                                        &bytes_to_forward,
                                        &mut console,
                                        &mut scheduler,
                                        now_unix_ms(),
                                    );
                                    let mut forwarded = Vec::new();
                                    let mut submitted_live_command = None;
                                    let mut pending_live_command = None;
                                    if let Some(runtime) = hosted.get_mut(&target) {
                                        let snapshot_live_command =
                                            if bytes_include_submit(&bytes_to_forward) {
                                                live_command_label_from_shell_snapshot(
                                                    runtime.screen_engine.state().active_snapshot(),
                                                )
                                            } else {
                                                None
                                            };
                                        forwarded = runtime.input_normalizer.normalize(
                                            &bytes_to_forward,
                                            runtime.screen_engine.application_cursor_keys(),
                                            now_unix_ms(),
                                        );
                                        submitted_live_command = runtime
                                            .command_tracker
                                            .observe(&bytes_to_forward)
                                            .and_then(|command| live_command_label(&command))
                                            .or(snapshot_live_command);
                                        pending_live_command =
                                            runtime.command_tracker.pending_live_command_label();
                                    }
                                    if let Some(command_title) = submitted_live_command {
                                        self.set_session_title(&target, command_title);
                                        live_surface.mark_known_live_command(target.clone());
                                        live_surface.mark_session_bootstrapping(
                                            target.clone(),
                                            now_unix_ms(),
                                        );
                                        scheduler.on_manual_switch(&mut console);
                                        suppress_scheduler_refresh = true;
                                        refresh_after_live_command = true;
                                    } else if let Some(command_title) = pending_live_command {
                                        self.set_session_title(&target, command_title);
                                    } else if !live_surface.is_known_live_command(&target) {
                                        self.restore_shell_session_title(&target);
                                    }
                                    if !forwarded.is_empty() {
                                        self.sessions.mark_input(&target);
                                        if let Some(runtime) = hosted.get_mut(&target) {
                                            runtime.handle.write_all(&forwarded)?;
                                        }
                                    }
                                }
                            }
                            if refresh_after_live_command {
                                if !self.can_redraw_sidebar_only(
                                    &previous_sidebar,
                                    &sidebar,
                                    &live_surface,
                                    &console,
                                    &command_prompt,
                                ) || !self.redraw_sidebar_only(
                                    &previous_sidebar,
                                    &mut renderer_state,
                                    &renderer,
                                    &console,
                                    &scheduler,
                                    &command_prompt,
                                    &mut sidebar,
                                )? {
                                    self.refresh_surface(
                                        RenderSurface::Workspace,
                                        &mut live_surface,
                                        &mut hosted,
                                        &mut renderer_state,
                                        &renderer,
                                        &console,
                                        &scheduler,
                                        &command_prompt,
                                        &mut sidebar,
                                    )?;
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
                    self.trace_output("workspace", &output_session, &bytes);
                    buffer_pending_runtime_events(&rx, &mut pending_events);
                    if pending_events.iter().any(runtime_event_is_input_priority) {
                        pending_events.push_front(RuntimeEvent::Output {
                            session: output_session,
                            bytes,
                        });
                        continue;
                    }
                    let mut should_passthrough_output = false;
                    let mut should_refresh_surface = false;
                    let mut snapshot_before_output = None;
                    let mut snapshot_after_output = None;
                    let mut first_substantive_output = false;

                    if let Some(runtime) = hosted.get_mut(&output_session) {
                        snapshot_before_output =
                            Some(runtime.screen_engine.state().active_snapshot().clone());
                        let substantive_output = output_is_substantive(&bytes);
                        first_substantive_output = substantive_output
                            && self
                                .sessions
                                .get(&output_session)
                                .and_then(|record| record.last_output_at_unix_ms)
                                .is_none();
                        let mut cleared_live_command = false;
                        if substantive_output {
                            self.sessions.mark_output(&output_session);
                        }
                        runtime.transcript.record_output(&bytes);
                        let replies = runtime.screen_engine.feed_and_collect_replies(&bytes);
                        snapshot_after_output =
                            Some(runtime.screen_engine.state().active_snapshot().clone());
                        let release_detected = looks_like_terminal_release_output(&bytes);
                        let title_reconciled = self
                            .reconcile_session_title_with_foreground_process(
                                &output_session,
                                runtime,
                                &mut live_surface,
                                now_unix_ms(),
                            )?;
                        if release_detected {
                            live_surface.clear_session_bootstrapping(&output_session);
                            suppress_scheduler_refresh = true;
                        }
                        if live_surface.is_known_live_command(&output_session)
                            && ((looks_like_shell_prompt_output(&bytes)
                                && !looks_like_terminal_takeover_output(&bytes)
                                && !looks_like_terminal_probe_output(&bytes)
                                && !live_surface.is_bootstrapping(&output_session, now_unix_ms()))
                                || release_detected)
                        {
                            live_surface.clear_known_live_command(&output_session);
                            cleared_live_command = true;
                        }
                        self.sessions
                            .update_screen_state(&output_session, runtime.screen_engine.state());
                        if cleared_live_command {
                            self.restore_shell_session_title(&output_session);
                        }
                        if !replies.is_empty() {
                            runtime.handle.write_all(&replies)?;
                        }
                        if substantive_output {
                            scheduler.on_session_output(
                                &output_session,
                                now_unix_ms(),
                                bytes.len(),
                            );
                        }
                        self.maybe_activate_live_surface_for_output(
                            &mut live_surface,
                            &mut hosted,
                            &console,
                            &command_prompt,
                            &sidebar,
                            &output_session,
                            &bytes,
                        )?;
                        let deactivated_live_surface = self
                            .maybe_deactivate_live_surface_after_output(
                                &mut live_surface,
                                &mut hosted,
                                &console,
                                &command_prompt,
                                &sidebar,
                                &output_session,
                            )?;
                        if deactivated_live_surface {
                            let shell_prompt_detected = looks_like_shell_prompt_output(&bytes);
                            should_refresh_surface =
                                !release_detected || substantive_output || shell_prompt_detected;
                        } else if native_fullscreen.is_active_for(&output_session) {
                            should_passthrough_output = true;
                        } else if native_fullscreen.is_active() {
                            should_refresh_surface = false;
                        } else if live_surface.is_live_for(&output_session)
                            && console.focused_session.as_ref() == Some(&output_session)
                        {
                            should_passthrough_output = true;
                        } else if focused_passthrough_output(
                            &live_surface,
                            &console,
                            &command_prompt,
                            &sidebar,
                            &output_session,
                        ) {
                            should_passthrough_output = true;
                        } else if !self
                            .focused_session_owns_passthrough_display(&live_surface, &console)
                        {
                            should_refresh_surface = true;
                        }
                        if title_reconciled && !native_fullscreen.is_active() {
                            should_refresh_surface = true;
                        }
                    }

                    if native_fullscreen.is_active_for(&output_session) {
                        self.write_live_surface_output(&bytes)?;
                    } else if should_passthrough_output {
                        let skip_snapshot_redraw = live_output_can_skip_snapshot_redraw(
                            &live_surface,
                            &output_session,
                            &bytes,
                            now_unix_ms(),
                        );
                        self.prepare_live_surface_passthrough(
                            &mut live_surface,
                            &mut hosted,
                            snapshot_before_output.as_ref(),
                            skip_snapshot_redraw,
                        )?;
                        self.write_live_surface_output_with_ui(
                            &bytes,
                            snapshot_before_output.as_ref(),
                            snapshot_after_output.as_ref(),
                            &mut live_surface,
                            &command_prompt,
                            &mut renderer_state,
                            &renderer,
                            &console,
                            &scheduler,
                            &mut sidebar,
                        )?;
                    } else if should_refresh_surface {
                        if first_substantive_output {
                            sidebar.rendered_overlay = None;
                        }
                        self.refresh_surface(
                            RenderSurface::Workspace,
                            &mut live_surface,
                            &mut hosted,
                            &mut renderer_state,
                            &renderer,
                            &console,
                            &scheduler,
                            &command_prompt,
                            &mut sidebar,
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
                        if native_fullscreen.is_active_for(&session) {
                            self.exit_native_fullscreen(
                                RenderSurface::Workspace,
                                &mut native_fullscreen,
                                &mut alternate_screen,
                                &mut live_surface,
                                &mut hosted,
                                &mut renderer_state,
                                &renderer,
                                &console,
                                &scheduler,
                                &command_prompt,
                                &mut sidebar,
                            )?;
                        }
                        self.refresh_surface(
                            RenderSurface::Workspace,
                            &mut live_surface,
                            &mut hosted,
                            &mut renderer_state,
                            &renderer,
                            &console,
                            &scheduler,
                            &command_prompt,
                            &mut sidebar,
                        )?;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => should_exit = true,
            }

            let now = now_unix_ms();

            if !native_fullscreen.is_active() && command_prompt.flush_picker_navigation_timeout(now)
            {
                self.refresh_surface(
                    RenderSurface::Workspace,
                    &mut live_surface,
                    &mut hosted,
                    &mut renderer_state,
                    &renderer,
                    &console,
                    &scheduler,
                    &command_prompt,
                    &mut sidebar,
                )?;
            }

            if !native_fullscreen.is_active()
                && sidebar.flush_navigation_timeout(&command_prompt, now)
            {
                self.refresh_surface(
                    RenderSurface::Workspace,
                    &mut live_surface,
                    &mut hosted,
                    &mut renderer_state,
                    &renderer,
                    &console,
                    &scheduler,
                    &command_prompt,
                    &mut sidebar,
                )?;
            }

            if let Some(target) = console.input_owner_session().cloned() {
                if native_fullscreen.is_active_for(&target) {
                } else if let Some(runtime) = hosted.get_mut(&target) {
                    let flushed = runtime.input_normalizer.flush_pending_escape_timeout(now);
                    if !flushed.is_empty() {
                        self.sessions.mark_input(&target);
                        runtime.handle.write_all(&flushed)?;
                    }
                }
            }

            if self.terminal.capture_resize()?.is_some() {
                if let Some(target) = native_fullscreen.session().cloned() {
                    let terminal_size = self.terminal.current_size_or_default();
                    if self.resize_hosted_session(&target, terminal_size, &mut hosted)? {
                        self.write_native_fullscreen_seed(&target, &hosted, terminal_size)?;
                    }
                } else {
                    self.refresh_surface(
                        RenderSurface::Workspace,
                        &mut live_surface,
                        &mut hosted,
                        &mut renderer_state,
                        &renderer,
                        &console,
                        &scheduler,
                        &command_prompt,
                        &mut sidebar,
                    )?;
                }
            }

            if !command_prompt.open
                && !self.focused_session_owns_passthrough_display(&live_surface, &console)
                && !native_fullscreen.is_active()
            {
                let suppress_scheduler_refresh = suppress_scheduler_refresh
                    || console
                        .focused_session
                        .as_ref()
                        .map(|session| {
                            live_surface.is_known_live_command(session)
                                || live_surface.is_bootstrapping(session, now)
                        })
                        .unwrap_or(false);
                let decision =
                    scheduler.decide_auto_switch(&mut console, self.sessions.list(), now_unix_ms());
                let waiting_count = scheduler.waiting_queue().entries().len();
                let waiting_addresses = scheduler.waiting_queue().addresses();
                if let Some(message) = background_wait_notice(
                    &last_waiting_addresses,
                    &waiting_addresses,
                    console.focused_session.as_ref(),
                ) {
                    command_prompt.set_passive_message(message);
                }
                if !matches!(decision.action, SchedulingAction::None)
                    || waiting_count != last_waiting_count
                {
                    if !suppress_scheduler_refresh {
                        self.refresh_surface(
                            RenderSurface::Workspace,
                            &mut live_surface,
                            &mut hosted,
                            &mut renderer_state,
                            &renderer,
                            &console,
                            &scheduler,
                            &command_prompt,
                            &mut sidebar,
                        )?;
                    }
                }
                last_waiting_count = waiting_count;
                last_waiting_addresses = waiting_addresses;
            }
        }

        if native_fullscreen.is_active() {
            alternate_screen.resume()?;
        }
        self.restore_terminal_screen()?;
        Ok(())
    }

    fn spawn_default_shell_session(
        &mut self,
        node_id: &str,
        size: crate::terminal::TerminalSize,
        sidebar_hidden: bool,
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
            sidebar_hidden,
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
        sidebar_hidden: bool,
        hosted: &mut HashMap<SessionAddress, HostedSession>,
        tx: &Sender<RuntimeEvent>,
    ) -> Result<SessionAddress, AppError> {
        let command_line = render_command_line(&program, &args);
        let session = self
            .sessions
            .create_local_session(node_id, title, command_line);
        let address = session.address().clone();
        let managed_size = workspace_viewport_size(size, sidebar_hidden);
        let screen_engine = TerminalEngine::new(managed_size);
        let handle = self.pty.spawn(
            address.clone(),
            SpawnRequest {
                program,
                args,
                size: PtySize::from(managed_size),
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
                transcript: TerminalTranscript::default(),
                input_normalizer: ForwardInputNormalizer::default(),
                command_tracker: ShellCommandTracker::default(),
                viewport_size: managed_size,
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
            None,
        )?;
        let mut last_waiting_count = scheduler.waiting_queue().entries().len();
        let mut last_waiting_addresses = scheduler.waiting_queue().addresses();

        let mut process_closed = false;
        loop {
            match rx.recv_timeout(EVENT_LOOP_TICK) {
                Ok(RuntimeEvent::Input(bytes)) => {
                    if let Some(target) = console.input_owner_session().cloned() {
                        self.sessions.mark_input(&target);
                        input_tracker.observe(&bytes, &mut console, &mut scheduler, now_unix_ms());
                        handle.write_all(&bytes)?;
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
                    scheduler.on_session_output(&output_session, now_unix_ms(), bytes.len());
                    self.render_console(
                        &mut renderer_state,
                        &renderer,
                        &console,
                        &scheduler,
                        Vec::new(),
                        None,
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
                let managed_size = managed_console_size(size);
                handle.resize(PtySize::from(managed_size))?;
                screen_engine.resize(managed_size);
                self.sessions
                    .update_screen_state(&session, screen_engine.state());
                self.render_console(
                    &mut renderer_state,
                    &renderer,
                    &console,
                    &scheduler,
                    Vec::new(),
                    None,
                    None,
                )?;
            }

            let decision =
                scheduler.decide_auto_switch(&mut console, self.sessions.list(), now_unix_ms());
            let waiting_count = scheduler.waiting_queue().entries().len();
            let waiting_addresses = scheduler.waiting_queue().addresses();
            let _ = background_wait_notice(
                &last_waiting_addresses,
                &waiting_addresses,
                console.focused_session.as_ref(),
            );
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
                    None,
                )?;
                last_waiting_count = waiting_count;
            }
            last_waiting_addresses = waiting_addresses;

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
        sidebar: Option<&mut SidebarState>,
    ) -> Result<(), AppError> {
        let mut sidebar = sidebar;
        let suppress_sidebar_diff = sidebar
            .as_deref()
            .map(|state| state.force_full_redraws > 0)
            .unwrap_or(false);
        let previous_sidebar_overlay = if suppress_sidebar_diff {
            None
        } else {
            sidebar
                .as_deref()
                .and_then(|state| state.rendered_overlay.clone())
        };
        let previous_frame_lines = renderer_state.previous_frame_lines().to_vec();
        let frame = renderer.render_with_state(
            renderer_state,
            console,
            &self.sessions.list(),
            RenderContext {
                waiting_count: scheduler.waiting_queue().entries().len(),
                overlay_lines,
                footer_width: self.terminal.current_size_or_default().cols as usize,
            },
        )?;
        let sidebar_state = self.build_sidebar_render_state(
            sidebar.as_deref_mut(),
            console,
            scheduler,
            command_prompt,
        );
        let (frame_text, cursor, cursor_visible, sidebar_overlay) =
            self.decorate_frame(&frame, command_prompt, sidebar_state.as_ref());
        let previous_frame_lines_arg =
            (!previous_frame_lines.is_empty()).then_some(previous_frame_lines.as_slice());
        let result = self.write_full_frame_at(
            &frame_text,
            previous_frame_lines_arg,
            sidebar_overlay.as_ref(),
            previous_sidebar_overlay.as_ref(),
            cursor,
            cursor_visible,
        );
        if result.is_ok() {
            renderer_state.update_frame_lines(split_frame_lines(&frame_text));
        }
        if let Some(sidebar_state) = sidebar.as_deref_mut() {
            sidebar_state.rendered_overlay = sidebar_overlay;
            sidebar_state.rendered_cursor_visible = cursor_visible;
            if result.is_ok() && sidebar_state.force_full_redraws > 0 {
                sidebar_state.force_full_redraws -= 1;
            }
        }
        result
    }

    fn write_live_surface_output(&self, bytes: &[u8]) -> Result<(), AppError> {
        let mut stdout = io::stdout().lock();
        stdout.write_all(bytes).map_err(|error| {
            AppError::Io("failed to write live surface output".to_string(), error)
        })?;
        stdout
            .flush()
            .map_err(|error| AppError::Io("failed to flush live surface output".to_string(), error))
    }

    fn write_ui_buffer_with_sync(
        &self,
        context: &str,
        buffer: &str,
        synchronized_updates: bool,
    ) -> Result<(), AppError> {
        let payload = build_ui_write_payload(buffer, synchronized_updates);
        let mut stdout = io::stdout().lock();
        stdout
            .write_all(payload.as_bytes())
            .map_err(|error| AppError::Io(format!("failed to write {context}"), error))?;
        stdout
            .flush()
            .map_err(|error| AppError::Io(format!("failed to flush {context}"), error))
    }

    fn write_ui_buffer(&self, context: &str, buffer: &str) -> Result<(), AppError> {
        self.write_ui_buffer_with_sync(context, buffer, true)
    }

    fn write_live_surface_output_with_ui(
        &self,
        bytes: &[u8],
        snapshot_before_output: Option<&crate::terminal::ScreenSnapshot>,
        snapshot_after_output: Option<&crate::terminal::ScreenSnapshot>,
        live_surface: &mut LiveSurfaceState,
        command_prompt: &CommandPromptState,
        renderer_state: &mut RendererState,
        renderer: &Renderer,
        console: &ConsoleState,
        scheduler: &SchedulerState,
        sidebar: &mut SidebarState,
    ) -> Result<(), AppError> {
        let batches = extract_live_output_batches(
            bytes,
            &mut live_surface.pending_sync_marker_bytes,
            &mut live_surface.pending_agent_output_batch,
            &mut live_surface.agent_sync_batch_open,
        );
        let filtered_bytes: Vec<u8> = batches.into_iter().flatten().collect();
        let workspace_buffer =
            self.build_live_surface_snapshot_delta(snapshot_before_output, snapshot_after_output);
        if filtered_bytes.is_empty() && workspace_buffer.is_none() {
            return Ok(());
        }

        // Live passthrough can repaint arbitrary cells, so the cached normal-frame diff
        // is no longer trustworthy once we hand the terminal to the agent.
        renderer_state.clear_frame_lines();
        let force_full_sidebar_redraw = live_output_requires_full_sidebar_redraw(&filtered_bytes)
            || live_output_requires_full_sidebar_redraw_from_snapshots(
                snapshot_before_output,
                snapshot_after_output,
            );
        let ui = self.build_live_surface_ui_buffer(
            live_surface,
            command_prompt,
            renderer_state,
            renderer,
            console,
            scheduler,
            sidebar,
            live_surface.overlay_rows,
            force_full_sidebar_redraw,
            None,
        )?;
        let mut payload = workspace_buffer.unwrap_or_default();
        payload.push_str(&ui.buffer);
        if !payload.is_empty() {
            self.write_ui_buffer("live surface snapshot delta with chrome", &payload)?;
            live_surface.chrome_visible = true;
            live_surface.overlay_rows = ui.overlay_rows;
            live_surface.sidebar_overlay = ui.sidebar_overlay;
            live_surface.separator_line = ui.separator_line;
            live_surface.keys_line = ui.keys_line;
            live_surface.status_line = ui.status_line;
        } else if filtered_bytes.is_empty() {
            return Ok(());
        } else {
            self.write_live_surface_output(&filtered_bytes)?;
        }

        Ok(())
    }

    fn build_live_surface_snapshot_delta(
        &self,
        before: Option<&crate::terminal::ScreenSnapshot>,
        after: Option<&crate::terminal::ScreenSnapshot>,
    ) -> Option<String> {
        let (before, after) = match (before, after) {
            (Some(before), Some(after)) => (before, after),
            _ => return None,
        };

        let full_redraw = before.size != after.size
            || before.alternate_screen != after.alternate_screen
            || before.scroll_top != after.scroll_top
            || before.scroll_bottom != after.scroll_bottom;
        let mut buffer = String::from("\x1b[?25l");
        for (index, line) in after.styled_lines.iter().enumerate() {
            if !full_redraw
                && before
                    .styled_lines
                    .get(index)
                    .map(|previous| previous == line)
                    .unwrap_or(false)
            {
                continue;
            }
            let row = index.saturating_add(1);
            buffer.push_str(&format!("\x1b[{row};1H{line}\x1b[0m"));
        }

        let cursor_row = after.cursor_row.saturating_add(1);
        let cursor_col = after.cursor_col.saturating_add(1);
        let cursor_visibility = if after.cursor_visible {
            "\x1b[?25h"
        } else {
            "\x1b[?25l"
        };
        let scroll_region = format!(
            "\x1b[{};{}r",
            after.scroll_top.saturating_add(1),
            after.scroll_bottom.saturating_add(1)
        );
        buffer.push_str(&format!(
            "{scroll_region}\x1b[{cursor_row};{cursor_col}H{}{cursor_visibility}",
            after.active_style_ansi
        ));

        Some(buffer)
    }

    fn write_live_surface_snapshot(
        &self,
        snapshot: &crate::terminal::ScreenSnapshot,
    ) -> Result<(), AppError> {
        self.write_terminal_snapshot("live surface snapshot", snapshot, false)
    }

    fn write_terminal_snapshot(
        &self,
        context: &str,
        snapshot: &crate::terminal::ScreenSnapshot,
        clear_screen: bool,
    ) -> Result<(), AppError> {
        let mut buffer = if clear_screen {
            CLEAR_SCROLLBACK_AND_SCREEN.to_string()
        } else {
            RESET_FRAME_CURSOR.to_string()
        };
        buffer.push_str("\x1b[?25l");
        for (index, line) in snapshot.styled_lines.iter().enumerate() {
            let row = index.saturating_add(1);
            buffer.push_str(&format!("\x1b[{row};1H{line}\x1b[0m"));
        }
        let cursor_row = snapshot.cursor_row.saturating_add(1);
        let cursor_col = snapshot.cursor_col.saturating_add(1);
        let cursor_visibility = if snapshot.cursor_visible {
            "\x1b[?25h"
        } else {
            "\x1b[?25l"
        };
        let scroll_region = format!(
            "\x1b[{};{}r",
            snapshot.scroll_top.saturating_add(1),
            snapshot.scroll_bottom.saturating_add(1)
        );
        buffer.push_str(&format!(
            "{scroll_region}\x1b[{cursor_row};{cursor_col}H{}{cursor_visibility}",
            snapshot.active_style_ansi
        ));

        self.write_ui_buffer_with_sync(context, &buffer, false)
    }

    fn write_native_fullscreen_snapshot_seed(
        &self,
        snapshot: &crate::terminal::ScreenSnapshot,
    ) -> Result<(), AppError> {
        let buffer = self.build_native_fullscreen_snapshot_seed(snapshot)?;
        self.write_ui_buffer_with_sync("native fullscreen replay seed", &buffer, false)
    }

    fn build_native_fullscreen_snapshot_seed(
        &self,
        snapshot: &crate::terminal::ScreenSnapshot,
    ) -> Result<String, AppError> {
        let mut buffer = format!("{CLEAR_SCROLLBACK_AND_SCREEN}\x1b[?25l");
        let seeded_lines = snapshot
            .styled_scrollback
            .iter()
            .cloned()
            .chain(snapshot.styled_lines.iter().cloned())
            .collect::<Vec<_>>();
        if !seeded_lines.is_empty() {
            buffer.push_str(&seeded_lines.join("\r\n"));
        }
        let cursor_row = snapshot.cursor_row.saturating_add(1);
        let cursor_col = snapshot.cursor_col.saturating_add(1);
        let cursor_visibility = if snapshot.cursor_visible {
            "\x1b[?25h"
        } else {
            "\x1b[?25l"
        };
        let scroll_region = format!(
            "\x1b[{};{}r",
            snapshot.scroll_top.saturating_add(1),
            snapshot.scroll_bottom.saturating_add(1)
        );
        buffer.push_str(&format!(
            "{scroll_region}\x1b[{cursor_row};{cursor_col}H{}{cursor_visibility}",
            snapshot.active_style_ansi
        ));

        Ok(buffer)
    }

    fn build_live_surface_ui_buffer(
        &self,
        live_surface: &LiveSurfaceState,
        command_prompt: &CommandPromptState,
        renderer_state: &mut RendererState,
        renderer: &Renderer,
        console: &ConsoleState,
        scheduler: &SchedulerState,
        sidebar: &mut SidebarState,
        previous_overlay_rows: usize,
        force_full_sidebar_redraw: bool,
        sidebar_damage: Option<(usize, usize)>,
    ) -> Result<LiveSurfaceUiBuffer, AppError> {
        let frame = renderer.render_with_state(
            renderer_state,
            console,
            &self.sessions.list(),
            RenderContext {
                waiting_count: scheduler.waiting_queue().entries().len(),
                overlay_lines: Vec::new(),
                footer_width: self.terminal.current_size_or_default().cols as usize,
            },
        )?;
        let size = self.terminal.current_size_or_default();
        let width = size.cols as usize;
        let overlay_lines = live_overlay_lines(
            command_prompt,
            self.sessions.list(),
            console.focused_session.as_ref(),
        );
        let keys_line = style_overlay_line(
            "keys: ^W cmd  ^B/^F switch  ^N new  ^O full on/off  ^L picker  ^X close  ^Q quit",
            width,
        );
        let status_text = command_prompt.status_line(&frame.bottom_line);
        let status_line = style_status_line(&status_text, width);
        let status_row = size.rows.max(1);
        let previous_sidebar_overlay = if force_full_sidebar_redraw {
            None
        } else {
            live_surface
                .sidebar_overlay
                .clone()
                .or_else(|| sidebar.rendered_overlay.clone())
        };
        let sidebar_state = self.build_sidebar_render_state(
            Some(sidebar),
            console,
            scheduler,
            Some(command_prompt),
        );
        let sidebar_overlay = self.build_sidebar_overlay(sidebar_state.as_ref());
        let previous_sidebar_overlay_ref = previous_sidebar_overlay.as_ref();
        let separator_line =
            style_footer_separator_line_for_sidebar(width, sidebar_overlay.as_ref());

        let available_overlay_rows = size.rows.saturating_sub(LIVE_SURFACE_STATUS_ROWS) as usize;
        let shown_overlay = if overlay_lines.len() > available_overlay_rows {
            overlay_lines[overlay_lines.len() - available_overlay_rows..].to_vec()
        } else {
            overlay_lines
        };
        let current_footer_rows = shown_overlay.len() + LIVE_SURFACE_STATUS_ROWS as usize;
        let previous_footer_rows = previous_overlay_rows + LIVE_SURFACE_STATUS_ROWS as usize;
        let footer_start_row = status_row
            .saturating_sub(current_footer_rows.saturating_sub(1) as u16)
            .max(1);
        let separator_row = footer_start_row;
        let overlay_start_row = separator_row.saturating_add(1);
        let keys_row = status_row.saturating_sub(1);
        let mut overlay_buffer = String::new();
        let previous_footer_start_row = status_row
            .saturating_sub(previous_footer_rows.saturating_sub(1) as u16)
            .max(1);
        if previous_footer_start_row < footer_start_row {
            for row in previous_footer_start_row..footer_start_row {
                overlay_buffer.push_str(&format!("\x1b[{row};1H\x1b[K"));
            }
        }
        if previous_footer_start_row != footer_start_row
            || live_surface.separator_line != separator_line
        {
            overlay_buffer.push_str(&format!("\x1b[{separator_row};1H{separator_line}"));
        }
        for (index, line) in shown_overlay.iter().enumerate() {
            let row = overlay_start_row.saturating_add(index as u16);
            overlay_buffer.push_str(&format!("\x1b[{row};1H{}", style_overlay_line(line, width)));
        }
        if previous_footer_start_row != footer_start_row || live_surface.keys_line != keys_line {
            overlay_buffer.push_str(&format!("\x1b[{keys_row};1H{keys_line}"));
        }
        if let Some(overlay) = sidebar_overlay.as_ref() {
            let sidebar_rows = status_row.saturating_sub(1) as usize;
            let redraw_all = previous_sidebar_overlay_ref
                .map(|previous| {
                    previous.separator_col != overlay.separator_col
                        || previous.content_col != overlay.content_col
                        || previous.lines.len() != overlay.lines.len()
                })
                .unwrap_or(true);
            for (index, sidebar_line) in overlay.lines.iter().take(sidebar_rows).enumerate() {
                let row = index + 1;
                let row_damaged = sidebar_damage
                    .map(|(start, end)| row >= start && row <= end)
                    .unwrap_or(false);
                if !redraw_all
                    && !row_damaged
                    && previous_sidebar_overlay_ref.and_then(|previous| previous.lines.get(index))
                        == Some(sidebar_line)
                {
                    continue;
                }
                let fill_style = leading_ansi_style_prefix(sidebar_line);
                overlay_buffer.push_str(&format!(
                    "\x1b[{row};{}H{}\x1b[{row};{}H{fill_style}\x1b[K\x1b[{row};{}H{}",
                    overlay.separator_col,
                    overlay.divider,
                    overlay.content_col,
                    overlay.content_col,
                    sidebar_line
                ));
            }
            if let Some(previous) = previous_sidebar_overlay_ref {
                if previous.lines.len() > overlay.lines.len() {
                    for row in overlay.lines.len() + 1..=previous.lines.len() {
                        let fill_style = previous
                            .lines
                            .get(row - 1)
                            .map(|line| leading_ansi_style_prefix(line))
                            .unwrap_or(ANSI_BG_SIDEBAR_ITEM);
                        overlay_buffer.push_str(&format!(
                            "\x1b[{row};{}H{}\x1b[{row};{}H{fill_style}\x1b[K",
                            previous.separator_col, previous.divider, previous.content_col,
                        ));
                    }
                }
            }
        }
        if live_surface.status_line != status_line {
            overlay_buffer.push_str(&format!("\x1b[{status_row};1H{status_line}"));
        }
        if !overlay_buffer.is_empty() {
            if let Some(snapshot) = self
                .sessions
                .get(&frame.input_owner_session)
                .and_then(|record| record.screen_state.as_ref())
                .map(|screen_state| screen_state.active_snapshot())
            {
                overlay_buffer.push_str(&live_surface_overlay_cursor_restore(snapshot));
            }
        }

        Ok(LiveSurfaceUiBuffer {
            buffer: overlay_buffer,
            overlay_rows: shown_overlay.len(),
            sidebar_overlay,
            separator_line,
            keys_line,
            status_line,
        })
    }

    fn write_live_surface_ui(
        &self,
        live_surface: &mut LiveSurfaceState,
        command_prompt: &CommandPromptState,
        renderer_state: &mut RendererState,
        renderer: &Renderer,
        console: &ConsoleState,
        scheduler: &SchedulerState,
        sidebar: &mut SidebarState,
        force_full_sidebar_redraw: bool,
        sidebar_damage: Option<(usize, usize)>,
    ) -> Result<(), AppError> {
        let ui = self.build_live_surface_ui_buffer(
            live_surface,
            command_prompt,
            renderer_state,
            renderer,
            console,
            scheduler,
            sidebar,
            live_surface.overlay_rows,
            force_full_sidebar_redraw,
            sidebar_damage,
        )?;

        self.write_ui_buffer_with_sync("live surface chrome", &ui.buffer, false)?;
        live_surface.chrome_visible = true;
        live_surface.overlay_rows = ui.overlay_rows;
        live_surface.sidebar_overlay = ui.sidebar_overlay;
        live_surface.separator_line = ui.separator_line;
        live_surface.keys_line = ui.keys_line;
        live_surface.status_line = ui.status_line;
        Ok(())
    }

    fn sync_live_surface(
        &mut self,
        live_surface: &mut LiveSurfaceState,
        hosted: &mut HashMap<SessionAddress, HostedSession>,
        console: &ConsoleState,
        command_prompt: &CommandPromptState,
        sidebar: &SidebarState,
    ) -> Result<(), AppError> {
        let desired_live_session =
            self.desired_live_surface_session(live_surface, console, command_prompt, sidebar);
        let desired_fullscreen_session = desired_live_session.clone().or_else(|| {
            self.focused_bootstrapping_session(live_surface, console, command_prompt, sidebar)
        });
        let terminal_size = self.terminal.current_size_or_default();
        let now = now_unix_ms();

        for (address, runtime) in hosted.iter_mut() {
            let focused_live_session = desired_live_session.as_ref() == Some(address);
            let keep_fullscreen = desired_fullscreen_session.as_ref() == Some(address)
                || self.session_prefers_fullscreen_background(live_surface, address);
            let target_size = live_surface_target_size(
                focused_live_session,
                keep_fullscreen,
                terminal_size,
                sidebar.hidden,
            );
            if runtime.viewport_size == target_size {
                continue;
            }

            runtime.handle.resize(PtySize::from(target_size))?;
            runtime.screen_engine.resize(target_size);
            runtime.viewport_size = target_size;
            self.sessions
                .update_screen_state(address, runtime.screen_engine.state());
        }

        if let Some(address) = desired_live_session {
            live_surface.set_display_session(Some(address), true, now);
        } else if let Some(address) = desired_fullscreen_session {
            live_surface.set_display_session(Some(address), false, now);
        } else {
            live_surface.set_display_session(None, false, now);
        }

        Ok(())
    }

    fn maybe_activate_live_surface_for_output(
        &mut self,
        live_surface: &mut LiveSurfaceState,
        hosted: &mut HashMap<SessionAddress, HostedSession>,
        console: &ConsoleState,
        command_prompt: &CommandPromptState,
        sidebar: &SidebarState,
        output_session: &SessionAddress,
        bytes: &[u8],
    ) -> Result<bool, AppError> {
        if console.focused_session.as_ref() != Some(output_session)
            || console.is_peeking()
            || live_surface.is_live_for(output_session)
        {
            return Ok(false);
        }

        let takeover_detected = looks_like_terminal_takeover_output(bytes);
        let probe_detected = looks_like_terminal_probe_output(bytes);
        let now = now_unix_ms();
        if probe_detected || takeover_detected {
            live_surface.mark_session_bootstrapping(output_session.clone(), now);
        }
        let is_bootstrapping = live_surface.is_bootstrapping(output_session, now);

        let prefers_live =
            self.session_prefers_live_surface(live_surface, output_session) || takeover_detected;
        if !prefers_live && !is_bootstrapping {
            return Ok(false);
        }

        self.sync_live_surface(live_surface, hosted, console, command_prompt, sidebar)?;
        Ok(takeover_detected)
    }

    fn focused_session_prefers_live_surface(
        &self,
        live_surface: &LiveSurfaceState,
        console: &ConsoleState,
    ) -> bool {
        let Some(focused) = console.focused_session.as_ref() else {
            return false;
        };

        self.session_prefers_live_surface(live_surface, focused)
    }

    #[cfg(test)]
    fn focused_session_supports_live_surface(&self, console: &ConsoleState) -> bool {
        let Some(focused) = console.focused_session.as_ref() else {
            return false;
        };
        self.session_supports_live_surface(focused)
    }

    fn session_supports_live_surface(&self, session: &SessionAddress) -> bool {
        let Some(record) = self.sessions.get(session) else {
            return false;
        };
        let Some(screen_state) = record.screen_state.as_ref() else {
            return false;
        };

        screen_state.alternate_screen_active
    }

    fn session_prefers_live_surface(
        &self,
        live_surface: &LiveSurfaceState,
        session: &SessionAddress,
    ) -> bool {
        live_surface.is_known_live_command(session) || self.session_supports_live_surface(session)
    }

    fn session_prefers_fullscreen_background(
        &self,
        live_surface: &LiveSurfaceState,
        session: &SessionAddress,
    ) -> bool {
        live_surface.is_bootstrapping(session, now_unix_ms())
            || self.session_prefers_live_surface(live_surface, session)
    }

    fn desired_live_surface_session(
        &self,
        live_surface: &LiveSurfaceState,
        console: &ConsoleState,
        command_prompt: &CommandPromptState,
        sidebar: &SidebarState,
    ) -> Option<SessionAddress> {
        let desired_live = console.focused_session.is_some()
            && !console.is_peeking()
            && !live_overlay_visible(command_prompt, sidebar)
            && self.focused_session_prefers_live_surface(live_surface, console);
        desired_live
            .then(|| console.focused_session.clone())
            .flatten()
    }

    fn focused_bootstrapping_session(
        &self,
        live_surface: &LiveSurfaceState,
        console: &ConsoleState,
        command_prompt: &CommandPromptState,
        sidebar: &SidebarState,
    ) -> Option<SessionAddress> {
        let focused = console.focused_session.as_ref()?;
        if console.is_peeking()
            || live_overlay_visible(command_prompt, sidebar)
            || !live_surface.is_bootstrapping(focused, now_unix_ms())
        {
            return None;
        }

        Some(focused.clone())
    }

    fn focused_session_owns_passthrough_display(
        &self,
        live_surface: &LiveSurfaceState,
        console: &ConsoleState,
    ) -> bool {
        let Some(focused) = console.focused_session.as_ref() else {
            return false;
        };

        !console.is_peeking() && live_surface.owns_display(focused, now_unix_ms())
    }

    fn maybe_deactivate_live_surface_after_output(
        &mut self,
        live_surface: &mut LiveSurfaceState,
        hosted: &mut HashMap<SessionAddress, HostedSession>,
        console: &ConsoleState,
        command_prompt: &CommandPromptState,
        sidebar: &SidebarState,
        output_session: &SessionAddress,
    ) -> Result<bool, AppError> {
        if !live_surface.is_live_for(output_session)
            || console.focused_session.as_ref() != Some(output_session)
            || self.session_prefers_live_surface(live_surface, output_session)
            || live_surface.is_bootstrapping(output_session, now_unix_ms())
        {
            return Ok(false);
        }

        self.sync_live_surface(live_surface, hosted, console, command_prompt, sidebar)?;
        Ok(true)
    }

    #[allow(clippy::too_many_arguments)]
    fn refresh_surface(
        &mut self,
        surface: RenderSurface,
        live_surface: &mut LiveSurfaceState,
        hosted: &mut HashMap<SessionAddress, HostedSession>,
        renderer_state: &mut RendererState,
        renderer: &Renderer,
        console: &ConsoleState,
        scheduler: &SchedulerState,
        command_prompt: &CommandPromptState,
        sidebar: &mut SidebarState,
    ) -> Result<(), AppError> {
        let had_live_surface_display = live_surface.display_may_be_live_owned();
        self.sync_live_surface(live_surface, hosted, console, command_prompt, sidebar)?;
        let owns_passthrough_display =
            self.focused_session_owns_passthrough_display(live_surface, console);
        if had_live_surface_display && !owns_passthrough_display {
            renderer_state.clear_frame_lines();
            sidebar.rendered_overlay = None;
            sidebar.force_full_redraws = sidebar.force_full_redraws.max(1);
        }
        if owns_passthrough_display {
            renderer_state.clear_frame_lines();
            if live_surface.pending_redraw {
                self.request_live_surface_redraw(live_surface, hosted)?;
            }
            self.write_live_surface_ui(
                live_surface,
                command_prompt,
                renderer_state,
                renderer,
                console,
                scheduler,
                sidebar,
                true,
                None,
            )?;
        } else {
            self.render_surface(
                surface,
                renderer_state,
                renderer,
                console,
                scheduler,
                command_prompt,
                sidebar,
            )?;
        }
        Ok(())
    }

    fn request_live_surface_redraw(
        &mut self,
        live_surface: &mut LiveSurfaceState,
        hosted: &mut HashMap<SessionAddress, HostedSession>,
    ) -> Result<(), AppError> {
        let Some(address) = live_surface.session.as_ref() else {
            live_surface.pending_redraw = false;
            return Ok(());
        };
        let Some(runtime) = hosted.get_mut(address) else {
            live_surface.pending_redraw = false;
            return Ok(());
        };
        let snapshot = runtime.screen_engine.state().active_snapshot().clone();
        self.complete_live_surface_redraw(live_surface, &snapshot)
    }

    fn complete_live_surface_redraw(
        &self,
        live_surface: &mut LiveSurfaceState,
        snapshot: &crate::terminal::ScreenSnapshot,
    ) -> Result<(), AppError> {
        self.write_live_surface_snapshot(snapshot)?;
        live_surface.chrome_visible = false;
        live_surface.overlay_rows = 0;
        live_surface.sidebar_overlay = None;
        live_surface.pending_redraw = false;
        Ok(())
    }

    fn prepare_live_surface_passthrough(
        &mut self,
        live_surface: &mut LiveSurfaceState,
        hosted: &mut HashMap<SessionAddress, HostedSession>,
        snapshot_before_output: Option<&crate::terminal::ScreenSnapshot>,
        skip_snapshot_redraw: bool,
    ) -> Result<(), AppError> {
        if live_surface.pending_redraw {
            if skip_snapshot_redraw {
                live_surface.chrome_visible = false;
                live_surface.overlay_rows = 0;
                live_surface.sidebar_overlay = None;
                live_surface.pending_redraw = false;
            } else if let Some(snapshot) = snapshot_before_output {
                self.complete_live_surface_redraw(live_surface, snapshot)?;
            } else {
                self.request_live_surface_redraw(live_surface, hosted)?;
            }
        }

        Ok(())
    }

    fn resize_hosted_session(
        &mut self,
        session: &SessionAddress,
        size: crate::terminal::TerminalSize,
        hosted: &mut HashMap<SessionAddress, HostedSession>,
    ) -> Result<bool, AppError> {
        let Some(runtime) = hosted.get_mut(session) else {
            return Ok(false);
        };
        if runtime.viewport_size == size {
            return Ok(true);
        }

        runtime.handle.resize(PtySize::from(size))?;
        runtime.screen_engine = runtime.transcript.rebuild_engine(size);
        runtime.viewport_size = size;
        self.sessions
            .update_screen_state(session, runtime.screen_engine.state());
        Ok(true)
    }

    fn write_native_fullscreen_seed(
        &self,
        session: &SessionAddress,
        hosted: &HashMap<SessionAddress, HostedSession>,
        terminal_size: crate::terminal::TerminalSize,
    ) -> Result<(), AppError> {
        if let Some(snapshot) = hosted
            .get(session)
            .map(|runtime| native_fullscreen_seed_snapshot(&runtime.transcript, terminal_size))
        {
            self.write_native_fullscreen_snapshot_seed(&snapshot)
        } else {
            self.write_ui_buffer_with_sync(
                "native fullscreen clear",
                "\x1b[2J\x1b[H\x1b[?25h",
                false,
            )
        }
    }

    fn enter_native_fullscreen(
        &mut self,
        native_fullscreen: &mut NativeFullscreenState,
        alternate_screen: &mut AlternateScreenGuard,
        live_surface: &mut LiveSurfaceState,
        hosted: &mut HashMap<SessionAddress, HostedSession>,
        console: &ConsoleState,
    ) -> Result<bool, AppError> {
        let Some(session) = console.focused_session.clone() else {
            return Ok(false);
        };
        if native_fullscreen.is_active_for(&session) {
            return Ok(false);
        }

        live_surface.set_display_session(None, false, now_unix_ms());
        let terminal_size = self.terminal.current_size_or_default();
        if !self.resize_hosted_session(&session, terminal_size, hosted)? {
            return Ok(false);
        }
        alternate_screen.suspend()?;
        self.write_native_fullscreen_seed(&session, hosted, terminal_size)?;
        native_fullscreen.activate(session.clone());
        Ok(true)
    }

    #[allow(clippy::too_many_arguments)]
    fn exit_native_fullscreen(
        &mut self,
        surface: RenderSurface,
        native_fullscreen: &mut NativeFullscreenState,
        alternate_screen: &mut AlternateScreenGuard,
        live_surface: &mut LiveSurfaceState,
        hosted: &mut HashMap<SessionAddress, HostedSession>,
        renderer_state: &mut RendererState,
        renderer: &Renderer,
        console: &ConsoleState,
        scheduler: &SchedulerState,
        command_prompt: &CommandPromptState,
        sidebar: &mut SidebarState,
    ) -> Result<bool, AppError> {
        if !native_fullscreen.is_active() {
            return Ok(false);
        }

        native_fullscreen.deactivate();
        alternate_screen.resume()?;
        renderer_state.clear_frame_lines();
        sidebar.rendered_overlay = None;
        sidebar.force_full_redraws = sidebar.force_full_redraws.max(1);
        self.refresh_surface(
            surface,
            live_surface,
            hosted,
            renderer_state,
            renderer,
            console,
            scheduler,
            command_prompt,
            sidebar,
        )?;
        Ok(true)
    }

    fn forward_native_fullscreen_input(
        &mut self,
        session: &SessionAddress,
        bytes: &[u8],
        hosted: &mut HashMap<SessionAddress, HostedSession>,
        console: &mut ConsoleState,
        scheduler: &mut SchedulerState,
        input_tracker: &mut InputTracker,
        command_prompt: &mut CommandPromptState,
        live_surface: &mut LiveSurfaceState,
    ) -> Result<(), AppError> {
        if bytes.is_empty() {
            return Ok(());
        }

        command_prompt.clear_message_on_forwarded_input(bytes);
        input_tracker.observe(bytes, console, scheduler, now_unix_ms());

        let mut forwarded = Vec::new();
        let mut submitted_live_command = None;
        let mut pending_live_command = None;
        if let Some(runtime) = hosted.get_mut(session) {
            let snapshot_live_command = if bytes_include_submit(bytes) {
                live_command_label_from_shell_snapshot(
                    runtime.screen_engine.state().active_snapshot(),
                )
            } else {
                None
            };
            forwarded = runtime.input_normalizer.normalize(
                bytes,
                runtime.screen_engine.application_cursor_keys(),
                now_unix_ms(),
            );
            submitted_live_command = runtime
                .command_tracker
                .observe(bytes)
                .and_then(|command| live_command_label(&command))
                .or(snapshot_live_command);
            pending_live_command = runtime.command_tracker.pending_live_command_label();
        }
        if let Some(command_title) = submitted_live_command {
            self.set_session_title(session, command_title);
            live_surface.mark_known_live_command(session.clone());
            live_surface.mark_session_bootstrapping(session.clone(), now_unix_ms());
            scheduler.on_manual_switch(console);
        } else if let Some(command_title) = pending_live_command {
            self.set_session_title(session, command_title);
        } else if !live_surface.is_known_live_command(session) {
            self.restore_shell_session_title(session);
        }
        if !forwarded.is_empty() {
            self.sessions.mark_input(session);
            if let Some(runtime) = hosted.get_mut(session) {
                runtime.handle.write_all(&forwarded)?;
            }
        }

        Ok(())
    }

    fn decorate_frame(
        &self,
        frame: &RenderFrame,
        command_prompt: Option<&CommandPromptState>,
        sidebar: Option<&SidebarRenderState>,
    ) -> (String, CursorPlacement, bool, Option<SidebarOverlay>) {
        let width = self.terminal.current_size_or_default().cols as usize;
        let sidebar_overlay = self.build_sidebar_overlay(sidebar);
        let mut lines =
            Vec::with_capacity(frame.styled_viewport_lines.len() + frame.overlay_lines.len() + 3);
        if !frame.top_line.is_empty() {
            lines.push(frame.top_line.clone());
        }
        lines.extend(frame.styled_viewport_lines.iter().cloned());
        if !frame.overlay_lines.is_empty() {
            lines.push(style_footer_separator_line_for_sidebar(
                width,
                sidebar_overlay.as_ref(),
            ));
        }
        lines.extend(
            frame
                .overlay_lines
                .iter()
                .map(|line| style_overlay_line(line, width)),
        );
        let status_text = command_prompt
            .map(|prompt| prompt.status_line(&frame.bottom_line))
            .unwrap_or_else(|| frame.bottom_line.clone());
        lines.push(style_status_line(&status_text, width));
        let cursor = command_prompt
            .filter(|prompt| prompt.open)
            .map(|prompt| self.command_bar_cursor(frame, prompt))
            .unwrap_or_else(|| self.frame_cursor(frame));
        let cursor_visible = if sidebar.map(|state| state.focused).unwrap_or(false) {
            false
        } else {
            command_prompt.map(|prompt| prompt.open).unwrap_or(false) || frame.cursor_visible
        };
        (lines.join("\r\n"), cursor, cursor_visible, sidebar_overlay)
    }

    fn build_sidebar_render_state(
        &self,
        sidebar: Option<&mut SidebarState>,
        console: &ConsoleState,
        scheduler: &SchedulerState,
        command_prompt: Option<&CommandPromptState>,
    ) -> Option<SidebarRenderState> {
        self.build_sidebar_render_state_at(
            sidebar,
            console,
            scheduler,
            command_prompt,
            now_unix_ms(),
        )
    }

    fn build_sidebar_render_state_at(
        &self,
        sidebar: Option<&mut SidebarState>,
        console: &ConsoleState,
        scheduler: &SchedulerState,
        command_prompt: Option<&CommandPromptState>,
        now_unix_ms: u128,
    ) -> Option<SidebarRenderState> {
        let _command_prompt = command_prompt?;
        let sidebar = sidebar?;
        if !sidebar.rendered() {
            return None;
        }

        let width = self.terminal.current_size_or_default().cols as usize;
        let (_, sidebar_width) = sidebar_layout(width, sidebar.hidden)?;
        let sidebar_render_width = sidebar_width.saturating_sub(1).max(1);
        let sessions = self.sessions.list();
        let active_sessions = picker_sessions(&sessions);
        sidebar.sync_selection(&sessions, console.focused_session.as_ref());
        let selected = sidebar.selected_session(&sessions, console.focused_session.as_ref());
        let waiting_addresses = scheduler.waiting_queue().addresses();
        let waiting = waiting_addresses
            .iter()
            .cloned()
            .collect::<HashSet<SessionAddress>>();

        let row_capacity = self
            .terminal
            .current_size_or_default()
            .rows
            .saturating_sub(1) as usize;

        let (collapsed, lines) = if sidebar.hidden {
            (
                true,
                build_collapsed_sidebar_lines(row_capacity, sidebar_render_width),
            )
        } else {
            let mut lines = Vec::new();
            lines.push(style_sidebar_header_line(
                " Sessions  [h] hide",
                sidebar_render_width,
            ));
            lines.push(style_sidebar_hint_line(
                " ← back  ↑↓ move  enter switch",
                sidebar_render_width,
            ));

            let detail_row = row_capacity.saturating_sub(1);
            let session_row_capacity = detail_row.saturating_sub(lines.len());
            for session in active_sessions.into_iter().take(session_row_capacity) {
                let is_selected = Some(session.address()) == selected.as_ref();
                lines.push(format_sidebar_item(
                    session,
                    is_selected,
                    waiting.contains(session.address()),
                    now_unix_ms,
                    sidebar_render_width,
                ));
            }

            while lines.len() < detail_row {
                lines.push(style_sidebar_item_line("", sidebar_render_width, false));
            }

            let detail_line = selected
                .as_ref()
                .and_then(|address| self.sessions.get(address))
                .map(|record| {
                    let detail_source =
                        build_sidebar_detail_source(record, waiting.contains(record.address()));
                    let detail = build_sidebar_detail_text(&detail_source, sidebar_render_width);
                    style_sidebar_detail_line(&detail, sidebar_render_width)
                })
                .unwrap_or_else(|| {
                    let detail = build_sidebar_detail_text("unknown", sidebar_render_width);
                    style_sidebar_detail_line(&detail, sidebar_render_width)
                });
            lines.push(detail_line);
            (false, lines)
        };

        Some(SidebarRenderState {
            collapsed,
            focused: sidebar.focused,
            width: sidebar_width,
            lines,
        })
    }

    fn build_sidebar_overlay(
        &self,
        sidebar: Option<&SidebarRenderState>,
    ) -> Option<SidebarOverlay> {
        let Some(sidebar) = sidebar else {
            return None;
        };
        let width = self.terminal.current_size_or_default().cols as usize;
        let Some((separator_col, _sidebar_width)) = sidebar_layout(width, sidebar.collapsed) else {
            return None;
        };
        let content_col = separator_col + 1;
        Some(SidebarOverlay {
            separator_col,
            content_col,
            divider: style_sidebar_divider(),
            lines: sidebar.lines.clone(),
        })
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
            .saturating_add(usize::from(!frame.overlay_lines.is_empty()))
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
        previous_frame_lines: Option<&[String]>,
        sidebar: Option<&SidebarOverlay>,
        previous_sidebar: Option<&SidebarOverlay>,
        cursor: CursorPlacement,
        cursor_visible: bool,
    ) -> Result<(), AppError> {
        let buffer = build_full_frame_buffer_with_sidebar_diff(
            frame_text,
            previous_frame_lines,
            sidebar,
            previous_sidebar,
            cursor,
            cursor_visible,
            self.terminal.current_size_or_default().rows,
        );
        self.write_ui_buffer("render frame", &buffer)
    }

    fn write_sidebar_overlay_only(
        &self,
        sidebar: &SidebarOverlay,
        previous_sidebar: Option<&SidebarOverlay>,
        cursor: CursorPlacement,
        active_style_ansi: &str,
        rendered_cursor_visible: bool,
    ) -> Result<(), AppError> {
        let buffer = build_sidebar_overlay_buffer(
            sidebar,
            previous_sidebar,
            cursor,
            active_style_ansi,
            rendered_cursor_visible,
        );
        self.write_ui_buffer("sidebar overlay", &buffer)
    }

    fn can_redraw_sidebar_only(
        &self,
        previous_sidebar: &SidebarState,
        sidebar: &SidebarState,
        live_surface: &LiveSurfaceState,
        console: &ConsoleState,
        command_prompt: &CommandPromptState,
    ) -> bool {
        if live_surface.display_may_be_live_owned()
            || self.focused_session_owns_passthrough_display(live_surface, console)
        {
            return false;
        }
        if previous_sidebar.hidden != sidebar.hidden
            || previous_sidebar.focused != sidebar.focused
            || sidebar.hidden
        {
            return false;
        }
        if command_prompt.open || command_prompt.has_blocking_overlay() {
            return false;
        }

        let width = self.terminal.current_size_or_default().cols as usize;
        let previous_layout = sidebar_layout(width, previous_sidebar.hidden);
        let current_layout = sidebar_layout(width, sidebar.hidden);
        sidebar.rendered()
            && previous_layout.is_some()
            && current_layout.is_some()
            && previous_layout == current_layout
    }

    fn redraw_sidebar_only(
        &self,
        previous_sidebar: &SidebarState,
        renderer_state: &mut RendererState,
        renderer: &Renderer,
        console: &ConsoleState,
        scheduler: &SchedulerState,
        command_prompt: &CommandPromptState,
        sidebar: &mut SidebarState,
    ) -> Result<bool, AppError> {
        let overlay_lines =
            command_prompt.overlay_lines(self.sessions.list(), console.focused_session.as_ref());
        let frame = renderer.render_with_state(
            renderer_state,
            console,
            &self.sessions.list(),
            RenderContext {
                waiting_count: scheduler.waiting_queue().entries().len(),
                overlay_lines,
                footer_width: self.terminal.current_size_or_default().cols as usize,
            },
        )?;
        let sidebar_state = self.build_sidebar_render_state(
            Some(sidebar),
            console,
            scheduler,
            Some(command_prompt),
        );
        let mut previous_sidebar = previous_sidebar.clone();
        let previous_sidebar_state = self.build_sidebar_render_state(
            Some(&mut previous_sidebar),
            console,
            scheduler,
            Some(command_prompt),
        );
        let Some(sidebar_overlay) = self.build_sidebar_overlay(sidebar_state.as_ref()) else {
            return Ok(false);
        };
        let previous_sidebar_overlay = self.build_sidebar_overlay(previous_sidebar_state.as_ref());
        let cursor = self.frame_cursor(&frame);
        let active_style_ansi = self
            .sessions
            .get(&frame.rendered_session)
            .and_then(|record| record.screen_state.as_ref())
            .map(|screen_state| screen_state.active_snapshot().active_style_ansi.as_str())
            .unwrap_or(ANSI_RESET);
        self.write_sidebar_overlay_only(
            &sidebar_overlay,
            previous_sidebar_overlay.as_ref(),
            cursor,
            active_style_ansi,
            previous_sidebar.rendered_cursor_visible,
        )?;
        sidebar.rendered_overlay = Some(sidebar_overlay);
        sidebar.rendered_cursor_visible = previous_sidebar.rendered_cursor_visible;
        if sidebar.force_full_redraws > 0 {
            sidebar.force_full_redraws -= 1;
        }
        Ok(true)
    }

    #[allow(clippy::too_many_arguments)]
    fn refresh_after_sidebar_navigation(
        &mut self,
        surface: RenderSurface,
        previous_sidebar: &SidebarState,
        live_surface: &mut LiveSurfaceState,
        hosted: &mut HashMap<SessionAddress, HostedSession>,
        renderer_state: &mut RendererState,
        renderer: &Renderer,
        console: &ConsoleState,
        scheduler: &SchedulerState,
        command_prompt: &CommandPromptState,
        sidebar: &mut SidebarState,
    ) -> Result<(), AppError> {
        if self.can_redraw_sidebar_only(
            previous_sidebar,
            sidebar,
            live_surface,
            console,
            command_prompt,
        ) && self.redraw_sidebar_only(
            previous_sidebar,
            renderer_state,
            renderer,
            console,
            scheduler,
            command_prompt,
            sidebar,
        )? {
            return Ok(());
        }

        self.refresh_surface(
            surface,
            live_surface,
            hosted,
            renderer_state,
            renderer,
            console,
            scheduler,
            command_prompt,
            sidebar,
        )
    }

    fn restore_terminal_screen(&mut self) -> Result<(), AppError> {
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

        let mut alternate_screen = self.terminal.enter_alternate_screen()?;
        let _raw_mode = self.terminal.enter_raw_mode()?;
        let mut console = ConsoleState::new("server-console");
        let mut scheduler = SchedulerState::new();
        let renderer = Renderer::new();
        let mut renderer_state = RendererState::default();
        let mut input_tracker = InputTracker::default();
        let mut command_prompt = CommandPromptState::default();
        let mut sidebar = SidebarState::default();
        let mut live_surface = LiveSurfaceState::default();
        let mut native_fullscreen = NativeFullscreenState::default();
        let mut hosted = HashMap::<SessionAddress, HostedSession>::new();

        let (tx, rx) = mpsc::channel();
        spawn_stdin_reader(tx.clone());

        self.refresh_surface(
            RenderSurface::Server,
            &mut live_surface,
            &mut hosted,
            &mut renderer_state,
            &renderer,
            &console,
            &scheduler,
            &command_prompt,
            &mut sidebar,
        )?;
        let mut last_waiting_count = scheduler.waiting_queue().entries().len();
        let mut last_waiting_addresses = scheduler.waiting_queue().addresses();
        let mut should_exit = false;
        let mut pending_events = VecDeque::new();

        while !should_exit {
            let mut suppress_scheduler_refresh = false;
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
                        if !native_fullscreen.is_active() {
                            self.refresh_surface(
                                RenderSurface::Server,
                                &mut live_surface,
                                &mut hosted,
                                &mut renderer_state,
                                &renderer,
                                &console,
                                &scheduler,
                                &command_prompt,
                                &mut sidebar,
                            )?;
                        }
                    }
                    Err(error) => {
                        let _ = write_delegated_spawn_response(
                            &mut connection.stream,
                            Err(error.to_string()),
                        );
                    }
                }
            }

            match next_runtime_event(&rx, &mut pending_events) {
                Ok(RuntimeEvent::Input(bytes)) => {
                    let input_received_at = now_unix_ms();
                    let passthrough_display =
                        self.focused_session_owns_passthrough_display(&live_surface, &console);
                    self.trace_input(
                        "server",
                        &bytes,
                        &command_prompt,
                        native_fullscreen.is_active(),
                        passthrough_display,
                    );
                    if native_fullscreen.is_active() {
                        let fullscreen_input = native_fullscreen.handle_input(&bytes);
                        if let Some(target) = native_fullscreen.session().cloned() {
                            self.forward_native_fullscreen_input(
                                &target,
                                &fullscreen_input.forwarded,
                                &mut hosted,
                                &mut console,
                                &mut scheduler,
                                &mut input_tracker,
                                &mut command_prompt,
                                &mut live_surface,
                            )?;
                        }
                        if fullscreen_input.exit_requested {
                            self.exit_native_fullscreen(
                                RenderSurface::Server,
                                &mut native_fullscreen,
                                &mut alternate_screen,
                                &mut live_surface,
                                &mut hosted,
                                &mut renderer_state,
                                &renderer,
                                &console,
                                &scheduler,
                                &command_prompt,
                                &mut sidebar,
                            )?;
                        }
                        continue;
                    }
                    let allow_interrupt_exit =
                        !self.focused_session_owns_passthrough_display(&live_surface, &console);
                    if let Some(outcome) = command_prompt.handle_picker_navigation(
                        &bytes,
                        &self.sessions.list(),
                        console.focused_session.as_ref(),
                        input_received_at,
                    ) {
                        match outcome {
                            PickerNavigationOutcome::Consumed => {}
                            PickerNavigationOutcome::Render => {
                                self.refresh_surface(
                                    RenderSurface::Server,
                                    &mut live_surface,
                                    &mut hosted,
                                    &mut renderer_state,
                                    &renderer,
                                    &console,
                                    &scheduler,
                                    &command_prompt,
                                    &mut sidebar,
                                )?;
                            }
                            PickerNavigationOutcome::Submit => {
                                if let Some(index) = command_prompt.selected_picker_index(
                                    &self.sessions.list(),
                                    console.focused_session.as_ref(),
                                ) {
                                    should_exit = self.apply_host_action(
                                        ConsoleAction::FocusIndex(index),
                                        runtime,
                                        terminal_snapshot.size,
                                        &mut live_surface,
                                        &mut hosted,
                                        &tx,
                                        &mut console,
                                        &mut scheduler,
                                        &mut renderer_state,
                                        &renderer,
                                        &mut command_prompt,
                                        &mut sidebar,
                                    )?;
                                } else {
                                    should_exit = self.apply_host_action(
                                        ConsoleAction::DismissOverlay,
                                        runtime,
                                        terminal_snapshot.size,
                                        &mut live_surface,
                                        &mut hosted,
                                        &tx,
                                        &mut console,
                                        &mut scheduler,
                                        &mut renderer_state,
                                        &renderer,
                                        &mut command_prompt,
                                        &mut sidebar,
                                    )?;
                                }
                            }
                        }
                    } else if matches!(
                        parse_console_action(
                            &bytes,
                            command_prompt.wants_escape_dismiss(),
                            allow_interrupt_exit,
                        ),
                        Some(ConsoleAction::QuitHost)
                    ) {
                        should_exit = true;
                    } else if let Some(outcome) = command_prompt.handle_input(&bytes) {
                        should_exit = self.apply_command_outcome(
                            outcome,
                            runtime,
                            terminal_snapshot.size,
                            &mut native_fullscreen,
                            &mut alternate_screen,
                            &mut live_surface,
                            &mut hosted,
                            &tx,
                            &mut console,
                            &mut scheduler,
                            &mut renderer_state,
                            &renderer,
                            &mut command_prompt,
                            &mut sidebar,
                            RenderSurface::Server,
                        )?;
                    } else if let Some(outcome) = {
                        let previous_sidebar = sidebar.clone();
                        sidebar
                            .handle_navigation(
                                &bytes,
                                &self.sessions.list(),
                                console.focused_session.as_ref(),
                                console.can_switch(),
                                &command_prompt,
                                input_received_at,
                            )
                            .map(|outcome| (previous_sidebar, outcome))
                    } {
                        let (previous_sidebar, outcome) = outcome;
                        match outcome {
                            SidebarNavigationOutcome::Consumed => {}
                            SidebarNavigationOutcome::Render => {
                                self.refresh_after_sidebar_navigation(
                                    RenderSurface::Server,
                                    &previous_sidebar,
                                    &mut live_surface,
                                    &mut hosted,
                                    &mut renderer_state,
                                    &renderer,
                                    &console,
                                    &scheduler,
                                    &command_prompt,
                                    &mut sidebar,
                                )?;
                            }
                            SidebarNavigationOutcome::Submit(address) => {
                                should_exit = self.apply_host_action(
                                    ConsoleAction::FocusAddress(address),
                                    runtime,
                                    terminal_snapshot.size,
                                    &mut live_surface,
                                    &mut hosted,
                                    &tx,
                                    &mut console,
                                    &mut scheduler,
                                    &mut renderer_state,
                                    &renderer,
                                    &mut command_prompt,
                                    &mut sidebar,
                                )?;
                            }
                        }
                    } else if let Some(action) = parse_console_action(
                        &bytes,
                        command_prompt.wants_escape_dismiss(),
                        allow_interrupt_exit,
                    ) {
                        match action {
                            ConsoleAction::PreviousSession
                                if command_prompt.move_picker_previous(
                                    &self.sessions.list(),
                                    console.focused_session.as_ref(),
                                ) =>
                            {
                                self.refresh_surface(
                                    RenderSurface::Server,
                                    &mut live_surface,
                                    &mut hosted,
                                    &mut renderer_state,
                                    &renderer,
                                    &console,
                                    &scheduler,
                                    &command_prompt,
                                    &mut sidebar,
                                )?;
                            }
                            ConsoleAction::NextSession
                                if command_prompt.move_picker_next(
                                    &self.sessions.list(),
                                    console.focused_session.as_ref(),
                                ) =>
                            {
                                self.refresh_surface(
                                    RenderSurface::Server,
                                    &mut live_surface,
                                    &mut hosted,
                                    &mut renderer_state,
                                    &renderer,
                                    &console,
                                    &scheduler,
                                    &command_prompt,
                                    &mut sidebar,
                                )?;
                            }
                            ConsoleAction::EnterNativeFullscreen => {
                                if self.enter_native_fullscreen(
                                    &mut native_fullscreen,
                                    &mut alternate_screen,
                                    &mut live_surface,
                                    &mut hosted,
                                    &console,
                                )? {
                                    command_prompt.clear_overlay();
                                }
                            }
                            _ => {
                                should_exit = self.apply_host_action(
                                    action,
                                    runtime,
                                    terminal_snapshot.size,
                                    &mut live_surface,
                                    &mut hosted,
                                    &tx,
                                    &mut console,
                                    &mut scheduler,
                                    &mut renderer_state,
                                    &renderer,
                                    &mut command_prompt,
                                    &mut sidebar,
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
                            &mut live_surface,
                            &mut hosted,
                            &tx,
                            &mut console,
                            &mut scheduler,
                            &mut renderer_state,
                            &renderer,
                            &mut command_prompt,
                            &mut sidebar,
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
                                &mut live_surface,
                                &mut hosted,
                                &tx,
                                &mut console,
                                &mut scheduler,
                                &mut renderer_state,
                                &renderer,
                                &mut command_prompt,
                                &mut sidebar,
                            )?;
                        } else {
                            should_exit = self.apply_host_action(
                                ConsoleAction::DismissOverlay,
                                runtime,
                                terminal_snapshot.size,
                                &mut live_surface,
                                &mut hosted,
                                &tx,
                                &mut console,
                                &mut scheduler,
                                &mut renderer_state,
                                &renderer,
                                &mut command_prompt,
                                &mut sidebar,
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
                                match outcome {
                                    PickerNavigationOutcome::Consumed => {}
                                    PickerNavigationOutcome::Render => {
                                        self.refresh_surface(
                                            RenderSurface::Server,
                                            &mut live_surface,
                                            &mut hosted,
                                            &mut renderer_state,
                                            &renderer,
                                            &console,
                                            &scheduler,
                                            &command_prompt,
                                            &mut sidebar,
                                        )?;
                                    }
                                    PickerNavigationOutcome::Submit => {
                                        if let Some(index) = command_prompt.selected_picker_index(
                                            &self.sessions.list(),
                                            console.focused_session.as_ref(),
                                        ) {
                                            should_exit = self.apply_host_action(
                                                ConsoleAction::FocusIndex(index),
                                                runtime,
                                                terminal_snapshot.size,
                                                &mut live_surface,
                                                &mut hosted,
                                                &tx,
                                                &mut console,
                                                &mut scheduler,
                                                &mut renderer_state,
                                                &renderer,
                                                &mut command_prompt,
                                                &mut sidebar,
                                            )?;
                                        } else {
                                            should_exit = self.apply_host_action(
                                                ConsoleAction::DismissOverlay,
                                                runtime,
                                                terminal_snapshot.size,
                                                &mut live_surface,
                                                &mut hosted,
                                                &tx,
                                                &mut console,
                                                &mut scheduler,
                                                &mut renderer_state,
                                                &renderer,
                                                &mut command_prompt,
                                                &mut sidebar,
                                            )?;
                                        }
                                    }
                                }
                            } else if let Some(outcome) = command_prompt.handle_input(&single) {
                                handled_control = true;
                                should_exit = self.apply_command_outcome(
                                    outcome,
                                    runtime,
                                    terminal_snapshot.size,
                                    &mut native_fullscreen,
                                    &mut alternate_screen,
                                    &mut live_surface,
                                    &mut hosted,
                                    &tx,
                                    &mut console,
                                    &mut scheduler,
                                    &mut renderer_state,
                                    &renderer,
                                    &mut command_prompt,
                                    &mut sidebar,
                                    RenderSurface::Server,
                                )?;
                            } else if let Some((previous_sidebar, outcome)) = {
                                let previous_sidebar = sidebar.clone();
                                sidebar
                                    .handle_navigation(
                                        &single,
                                        &self.sessions.list(),
                                        console.focused_session.as_ref(),
                                        console.can_switch(),
                                        &command_prompt,
                                        now_unix_ms(),
                                    )
                                    .map(|outcome| (previous_sidebar, outcome))
                            } {
                                handled_control = true;
                                match outcome {
                                    SidebarNavigationOutcome::Consumed => {}
                                    SidebarNavigationOutcome::Render => {
                                        self.refresh_after_sidebar_navigation(
                                            RenderSurface::Server,
                                            &previous_sidebar,
                                            &mut live_surface,
                                            &mut hosted,
                                            &mut renderer_state,
                                            &renderer,
                                            &console,
                                            &scheduler,
                                            &command_prompt,
                                            &mut sidebar,
                                        )?;
                                    }
                                    SidebarNavigationOutcome::Submit(address) => {
                                        should_exit = self.apply_host_action(
                                            ConsoleAction::FocusAddress(address),
                                            runtime,
                                            terminal_snapshot.size,
                                            &mut live_surface,
                                            &mut hosted,
                                            &tx,
                                            &mut console,
                                            &mut scheduler,
                                            &mut renderer_state,
                                            &renderer,
                                            &mut command_prompt,
                                            &mut sidebar,
                                        )?;
                                    }
                                }
                            } else if let Some(action) = parse_console_action(
                                &single,
                                command_prompt.wants_escape_dismiss(),
                                allow_interrupt_exit,
                            ) {
                                handled_control = true;
                                match action {
                                    ConsoleAction::PreviousSession
                                        if command_prompt.move_picker_previous(
                                            &self.sessions.list(),
                                            console.focused_session.as_ref(),
                                        ) =>
                                    {
                                        self.refresh_surface(
                                            RenderSurface::Server,
                                            &mut live_surface,
                                            &mut hosted,
                                            &mut renderer_state,
                                            &renderer,
                                            &console,
                                            &scheduler,
                                            &command_prompt,
                                            &mut sidebar,
                                        )?;
                                    }
                                    ConsoleAction::NextSession
                                        if command_prompt.move_picker_next(
                                            &self.sessions.list(),
                                            console.focused_session.as_ref(),
                                        ) =>
                                    {
                                        self.refresh_surface(
                                            RenderSurface::Server,
                                            &mut live_surface,
                                            &mut hosted,
                                            &mut renderer_state,
                                            &renderer,
                                            &console,
                                            &scheduler,
                                            &command_prompt,
                                            &mut sidebar,
                                        )?;
                                    }
                                    ConsoleAction::EnterNativeFullscreen => {
                                        handled_control = true;
                                        if self.enter_native_fullscreen(
                                            &mut native_fullscreen,
                                            &mut alternate_screen,
                                            &mut live_surface,
                                            &mut hosted,
                                            &console,
                                        )? {
                                            command_prompt.clear_overlay();
                                        }
                                    }
                                    _ => {
                                        should_exit = self.apply_host_action(
                                            action,
                                            runtime,
                                            terminal_snapshot.size,
                                            &mut live_surface,
                                            &mut hosted,
                                            &tx,
                                            &mut console,
                                            &mut scheduler,
                                            &mut renderer_state,
                                            &renderer,
                                            &mut command_prompt,
                                            &mut sidebar,
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
                                    &mut live_surface,
                                    &mut hosted,
                                    &tx,
                                    &mut console,
                                    &mut scheduler,
                                    &mut renderer_state,
                                    &renderer,
                                    &mut command_prompt,
                                    &mut sidebar,
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
                                        &mut live_surface,
                                        &mut hosted,
                                        &tx,
                                        &mut console,
                                        &mut scheduler,
                                        &mut renderer_state,
                                        &renderer,
                                        &mut command_prompt,
                                        &mut sidebar,
                                    )?;
                                } else {
                                    should_exit = self.apply_host_action(
                                        ConsoleAction::DismissOverlay,
                                        runtime,
                                        terminal_snapshot.size,
                                        &mut live_surface,
                                        &mut hosted,
                                        &tx,
                                        &mut console,
                                        &mut scheduler,
                                        &mut renderer_state,
                                        &renderer,
                                        &mut command_prompt,
                                        &mut sidebar,
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
                            let bytes_to_forward = if sidebar.modal_active(&command_prompt) {
                                Vec::new()
                            } else if handled_control {
                                residual
                            } else {
                                bytes
                            };
                            let mut refresh_after_live_command = false;
                            let previous_sidebar = sidebar.clone();
                            if let Some(target) = console.input_owner_session().cloned() {
                                if !bytes_to_forward.is_empty() {
                                    command_prompt
                                        .clear_message_on_forwarded_input(&bytes_to_forward);
                                    input_tracker.observe(
                                        &bytes_to_forward,
                                        &mut console,
                                        &mut scheduler,
                                        now_unix_ms(),
                                    );
                                    let mut forwarded = Vec::new();
                                    let mut submitted_live_command = None;
                                    let mut pending_live_command = None;
                                    if let Some(runtime) = hosted.get_mut(&target) {
                                        let snapshot_live_command =
                                            if bytes_include_submit(&bytes_to_forward) {
                                                live_command_label_from_shell_snapshot(
                                                    runtime.screen_engine.state().active_snapshot(),
                                                )
                                            } else {
                                                None
                                            };
                                        forwarded = runtime.input_normalizer.normalize(
                                            &bytes_to_forward,
                                            runtime.screen_engine.application_cursor_keys(),
                                            now_unix_ms(),
                                        );
                                        submitted_live_command = runtime
                                            .command_tracker
                                            .observe(&bytes_to_forward)
                                            .and_then(|command| live_command_label(&command))
                                            .or(snapshot_live_command);
                                        pending_live_command =
                                            runtime.command_tracker.pending_live_command_label();
                                    }
                                    if let Some(command_title) = submitted_live_command {
                                        self.set_session_title(&target, command_title);
                                        live_surface.mark_known_live_command(target.clone());
                                        live_surface.mark_session_bootstrapping(
                                            target.clone(),
                                            now_unix_ms(),
                                        );
                                        scheduler.on_manual_switch(&mut console);
                                        suppress_scheduler_refresh = true;
                                        refresh_after_live_command = true;
                                    } else if let Some(command_title) = pending_live_command {
                                        self.set_session_title(&target, command_title);
                                    } else if !live_surface.is_known_live_command(&target) {
                                        self.restore_shell_session_title(&target);
                                    }
                                    if !forwarded.is_empty() {
                                        self.sessions.mark_input(&target);
                                        if let Some(runtime) = hosted.get_mut(&target) {
                                            runtime.handle.write_all(&forwarded)?;
                                        }
                                    }
                                }
                            }
                            if refresh_after_live_command {
                                if !self.can_redraw_sidebar_only(
                                    &previous_sidebar,
                                    &sidebar,
                                    &live_surface,
                                    &console,
                                    &command_prompt,
                                ) || !self.redraw_sidebar_only(
                                    &previous_sidebar,
                                    &mut renderer_state,
                                    &renderer,
                                    &console,
                                    &scheduler,
                                    &command_prompt,
                                    &mut sidebar,
                                )? {
                                    self.refresh_surface(
                                        RenderSurface::Server,
                                        &mut live_surface,
                                        &mut hosted,
                                        &mut renderer_state,
                                        &renderer,
                                        &console,
                                        &scheduler,
                                        &command_prompt,
                                        &mut sidebar,
                                    )?;
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
                    self.trace_output("server", &output_session, &bytes);
                    buffer_pending_runtime_events(&rx, &mut pending_events);
                    if pending_events.iter().any(runtime_event_is_input_priority) {
                        pending_events.push_front(RuntimeEvent::Output {
                            session: output_session,
                            bytes,
                        });
                        continue;
                    }
                    let mut should_passthrough_output = false;
                    let mut should_refresh_surface = false;
                    let mut snapshot_before_output = None;
                    let mut snapshot_after_output = None;
                    let mut first_substantive_output = false;

                    if let Some(runtime) = hosted.get_mut(&output_session) {
                        snapshot_before_output =
                            Some(runtime.screen_engine.state().active_snapshot().clone());
                        let substantive_output = output_is_substantive(&bytes);
                        first_substantive_output = substantive_output
                            && self
                                .sessions
                                .get(&output_session)
                                .and_then(|record| record.last_output_at_unix_ms)
                                .is_none();
                        let mut cleared_live_command = false;
                        if substantive_output {
                            self.sessions.mark_output(&output_session);
                        }
                        runtime.transcript.record_output(&bytes);
                        let replies = runtime.screen_engine.feed_and_collect_replies(&bytes);
                        snapshot_after_output =
                            Some(runtime.screen_engine.state().active_snapshot().clone());
                        let release_detected = looks_like_terminal_release_output(&bytes);
                        let title_reconciled = self
                            .reconcile_session_title_with_foreground_process(
                                &output_session,
                                runtime,
                                &mut live_surface,
                                now_unix_ms(),
                            )?;
                        if release_detected {
                            live_surface.clear_session_bootstrapping(&output_session);
                            suppress_scheduler_refresh = true;
                        }
                        if live_surface.is_known_live_command(&output_session)
                            && ((looks_like_shell_prompt_output(&bytes)
                                && !looks_like_terminal_takeover_output(&bytes)
                                && !looks_like_terminal_probe_output(&bytes)
                                && !live_surface.is_bootstrapping(&output_session, now_unix_ms()))
                                || release_detected)
                        {
                            live_surface.clear_known_live_command(&output_session);
                            cleared_live_command = true;
                        }
                        self.sessions
                            .update_screen_state(&output_session, runtime.screen_engine.state());
                        if cleared_live_command {
                            self.restore_shell_session_title(&output_session);
                        }
                        if !replies.is_empty() {
                            runtime.handle.write_all(&replies)?;
                        }
                        if substantive_output {
                            scheduler.on_session_output(
                                &output_session,
                                now_unix_ms(),
                                bytes.len(),
                            );
                        }
                        self.maybe_activate_live_surface_for_output(
                            &mut live_surface,
                            &mut hosted,
                            &console,
                            &command_prompt,
                            &sidebar,
                            &output_session,
                            &bytes,
                        )?;
                        let deactivated_live_surface = self
                            .maybe_deactivate_live_surface_after_output(
                                &mut live_surface,
                                &mut hosted,
                                &console,
                                &command_prompt,
                                &sidebar,
                                &output_session,
                            )?;
                        if deactivated_live_surface {
                            let shell_prompt_detected = looks_like_shell_prompt_output(&bytes);
                            should_refresh_surface =
                                !release_detected || substantive_output || shell_prompt_detected;
                        } else if native_fullscreen.is_active_for(&output_session) {
                            should_passthrough_output = true;
                        } else if native_fullscreen.is_active() {
                            should_refresh_surface = false;
                        } else if live_surface.is_live_for(&output_session)
                            && console.focused_session.as_ref() == Some(&output_session)
                        {
                            should_passthrough_output = true;
                        } else if focused_passthrough_output(
                            &live_surface,
                            &console,
                            &command_prompt,
                            &sidebar,
                            &output_session,
                        ) {
                            should_passthrough_output = true;
                        } else if !self
                            .focused_session_owns_passthrough_display(&live_surface, &console)
                        {
                            should_refresh_surface = true;
                        }
                        if title_reconciled && !native_fullscreen.is_active() {
                            should_refresh_surface = true;
                        }
                    }

                    if native_fullscreen.is_active_for(&output_session) {
                        self.write_live_surface_output(&bytes)?;
                    } else if should_passthrough_output {
                        let skip_snapshot_redraw = live_output_can_skip_snapshot_redraw(
                            &live_surface,
                            &output_session,
                            &bytes,
                            now_unix_ms(),
                        );
                        self.prepare_live_surface_passthrough(
                            &mut live_surface,
                            &mut hosted,
                            snapshot_before_output.as_ref(),
                            skip_snapshot_redraw,
                        )?;
                        self.write_live_surface_output_with_ui(
                            &bytes,
                            snapshot_before_output.as_ref(),
                            snapshot_after_output.as_ref(),
                            &mut live_surface,
                            &command_prompt,
                            &mut renderer_state,
                            &renderer,
                            &console,
                            &scheduler,
                            &mut sidebar,
                        )?;
                    } else if should_refresh_surface {
                        if first_substantive_output {
                            sidebar.rendered_overlay = None;
                        }
                        self.refresh_surface(
                            RenderSurface::Server,
                            &mut live_surface,
                            &mut hosted,
                            &mut renderer_state,
                            &renderer,
                            &console,
                            &scheduler,
                            &command_prompt,
                            &mut sidebar,
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
                        if native_fullscreen.is_active_for(&session) {
                            self.exit_native_fullscreen(
                                RenderSurface::Server,
                                &mut native_fullscreen,
                                &mut alternate_screen,
                                &mut live_surface,
                                &mut hosted,
                                &mut renderer_state,
                                &renderer,
                                &console,
                                &scheduler,
                                &command_prompt,
                                &mut sidebar,
                            )?;
                        }
                        self.refresh_surface(
                            RenderSurface::Server,
                            &mut live_surface,
                            &mut hosted,
                            &mut renderer_state,
                            &renderer,
                            &console,
                            &scheduler,
                            &command_prompt,
                            &mut sidebar,
                        )?;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => should_exit = true,
            }

            let now = now_unix_ms();

            if !native_fullscreen.is_active() && command_prompt.flush_picker_navigation_timeout(now)
            {
                self.refresh_surface(
                    RenderSurface::Server,
                    &mut live_surface,
                    &mut hosted,
                    &mut renderer_state,
                    &renderer,
                    &console,
                    &scheduler,
                    &command_prompt,
                    &mut sidebar,
                )?;
            }

            if !native_fullscreen.is_active()
                && sidebar.flush_navigation_timeout(&command_prompt, now)
            {
                self.refresh_surface(
                    RenderSurface::Server,
                    &mut live_surface,
                    &mut hosted,
                    &mut renderer_state,
                    &renderer,
                    &console,
                    &scheduler,
                    &command_prompt,
                    &mut sidebar,
                )?;
            }

            if let Some(target) = console.input_owner_session().cloned() {
                if native_fullscreen.is_active_for(&target) {
                } else if let Some(runtime) = hosted.get_mut(&target) {
                    let flushed = runtime.input_normalizer.flush_pending_escape_timeout(now);
                    if !flushed.is_empty() {
                        self.sessions.mark_input(&target);
                        runtime.handle.write_all(&flushed)?;
                    }
                }
            }

            server_runtime.expire_stale_nodes(now_unix_ms());

            if self.terminal.capture_resize()?.is_some() {
                if let Some(target) = native_fullscreen.session().cloned() {
                    let terminal_size = self.terminal.current_size_or_default();
                    if self.resize_hosted_session(&target, terminal_size, &mut hosted)? {
                        self.write_native_fullscreen_seed(&target, &hosted, terminal_size)?;
                    }
                } else {
                    self.refresh_surface(
                        RenderSurface::Server,
                        &mut live_surface,
                        &mut hosted,
                        &mut renderer_state,
                        &renderer,
                        &console,
                        &scheduler,
                        &command_prompt,
                        &mut sidebar,
                    )?;
                }
            }

            if !command_prompt.open
                && !self.focused_session_owns_passthrough_display(&live_surface, &console)
                && !native_fullscreen.is_active()
            {
                let suppress_scheduler_refresh = suppress_scheduler_refresh
                    || console
                        .focused_session
                        .as_ref()
                        .map(|session| {
                            live_surface.is_known_live_command(session)
                                || live_surface.is_bootstrapping(session, now)
                        })
                        .unwrap_or(false);
                let decision =
                    scheduler.decide_auto_switch(&mut console, self.sessions.list(), now_unix_ms());
                let waiting_count = scheduler.waiting_queue().entries().len();
                let waiting_addresses = scheduler.waiting_queue().addresses();
                if let Some(message) = background_wait_notice(
                    &last_waiting_addresses,
                    &waiting_addresses,
                    console.focused_session.as_ref(),
                ) {
                    command_prompt.set_passive_message(message);
                }
                if !matches!(decision.action, SchedulingAction::None)
                    || waiting_count != last_waiting_count
                {
                    if !suppress_scheduler_refresh {
                        self.refresh_surface(
                            RenderSurface::Server,
                            &mut live_surface,
                            &mut hosted,
                            &mut renderer_state,
                            &renderer,
                            &console,
                            &scheduler,
                            &command_prompt,
                            &mut sidebar,
                        )?;
                    }
                }
                last_waiting_count = waiting_count;
                last_waiting_addresses = waiting_addresses;
            }
        }

        if native_fullscreen.is_active() {
            alternate_screen.resume()?;
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
            false,
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

    fn set_session_title(&mut self, address: &SessionAddress, title: impl Into<String>) {
        let _ = self.sessions.set_title(address, title);
    }

    fn restore_shell_session_title(&mut self, address: &SessionAddress) {
        let Some(title) = self
            .sessions
            .get(address)
            .map(|record| shell_title_from_command_line(&record.command_line))
        else {
            return;
        };
        self.set_session_title(address, title);
    }

    fn reconcile_session_title_with_foreground_process(
        &mut self,
        address: &SessionAddress,
        runtime: &HostedSession,
        live_surface: &mut LiveSurfaceState,
        now_unix_ms: u128,
    ) -> Result<bool, AppError> {
        let Some((current_title, command_line)) = self
            .sessions
            .get(address)
            .map(|record| (record.title.clone(), record.command_line.clone()))
        else {
            return Ok(false);
        };

        let foreground_name = match runtime.handle.foreground_process_name() {
            Ok(name) => name,
            Err(error) if nonfatal_foreground_process_inspection_error(&error) => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        let bootstrapping = live_surface.is_bootstrapping(address, now_unix_ms);

        match foreground_title_decision(&command_line, foreground_name.as_deref(), bootstrapping) {
            ForegroundTitleDecision::Keep => Ok(false),
            ForegroundTitleDecision::SetLive(title) => {
                let changed =
                    current_title != title || !live_surface.is_known_live_command(address);
                self.set_session_title(address, title);
                live_surface.mark_known_live_command(address.clone());
                Ok(changed)
            }
            ForegroundTitleDecision::RestoreShell(title) => {
                let changed = current_title != title || live_surface.is_known_live_command(address);
                self.set_session_title(address, title);
                live_surface.clear_known_live_command(address);
                Ok(changed)
            }
        }
    }

    fn warm_up_shell_session(
        &mut self,
        rx: &mpsc::Receiver<RuntimeEvent>,
        hosted: &mut HashMap<SessionAddress, HostedSession>,
        max_wait: Duration,
    ) -> Result<(), AppError> {
        let deadline = Instant::now() + max_wait;

        while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
            if remaining.is_zero() {
                break;
            }

            match rx.recv_timeout(remaining.min(Duration::from_millis(20))) {
                Ok(RuntimeEvent::Output {
                    session: output_session,
                    bytes,
                }) => {
                    let Some(runtime) = hosted.get_mut(&output_session) else {
                        continue;
                    };
                    let substantive_output = output_is_substantive(&bytes);
                    if substantive_output {
                        self.sessions.mark_output(&output_session);
                    }
                    runtime.transcript.record_output(&bytes);
                    let replies = runtime.screen_engine.feed_and_collect_replies(&bytes);
                    self.sessions
                        .update_screen_state(&output_session, runtime.screen_engine.state());
                    if !replies.is_empty() {
                        runtime.handle.write_all(&replies)?;
                    }
                    if looks_like_shell_prompt_output(&bytes) {
                        break;
                    }
                }
                Ok(RuntimeEvent::OutputClosed { .. }) => break,
                Ok(RuntimeEvent::InputClosed) => break,
                Ok(RuntimeEvent::Input(_)) => {}
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }

        Ok(())
    }

    fn render_workspace_console(
        &self,
        renderer_state: &mut RendererState,
        renderer: &Renderer,
        console: &ConsoleState,
        scheduler: &SchedulerState,
        command_prompt: &CommandPromptState,
        sidebar: &mut SidebarState,
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
            let status_line = command_prompt.status_line("focus: none | mode: workspace-idle");
            let (idle_frame, sidebar_overlay) = self.render_idle_frame(
                "workspace",
                active_count,
                waiting_count,
                &overlay_lines,
                &status_line,
                Some(sidebar),
                console,
                scheduler,
                Some(command_prompt),
            );
            let suppress_sidebar_diff = sidebar.force_full_redraws > 0;
            let previous_sidebar_overlay = if suppress_sidebar_diff {
                None
            } else {
                sidebar.rendered_overlay.clone()
            };
            self.write_full_frame_at(
                &idle_frame,
                None,
                sidebar_overlay.as_ref(),
                previous_sidebar_overlay.as_ref(),
                CursorPlacement { row: 0, col: 0 },
                true,
            )?;
            sidebar.rendered_overlay = sidebar_overlay;
            sidebar.rendered_cursor_visible = true;
            if sidebar.force_full_redraws > 0 {
                sidebar.force_full_redraws -= 1;
            }
            return Ok(());
        }

        self.render_console(
            renderer_state,
            renderer,
            console,
            scheduler,
            overlay_lines,
            Some(command_prompt),
            Some(sidebar),
        )
    }

    fn render_host_console(
        &self,
        renderer_state: &mut RendererState,
        renderer: &Renderer,
        console: &ConsoleState,
        scheduler: &SchedulerState,
        command_prompt: &CommandPromptState,
        sidebar: &mut SidebarState,
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
            let status_line = command_prompt.status_line("focus: none | mode: host-idle");
            let (idle_frame, sidebar_overlay) = self.render_idle_frame(
                "host",
                active_count,
                waiting_count,
                &overlay_lines,
                &status_line,
                Some(sidebar),
                console,
                scheduler,
                Some(command_prompt),
            );
            let suppress_sidebar_diff = sidebar.force_full_redraws > 0;
            let previous_sidebar_overlay = if suppress_sidebar_diff {
                None
            } else {
                sidebar.rendered_overlay.clone()
            };
            self.write_full_frame_at(
                &idle_frame,
                None,
                sidebar_overlay.as_ref(),
                previous_sidebar_overlay.as_ref(),
                CursorPlacement { row: 0, col: 0 },
                true,
            )?;
            sidebar.rendered_overlay = sidebar_overlay;
            sidebar.rendered_cursor_visible = true;
            if sidebar.force_full_redraws > 0 {
                sidebar.force_full_redraws -= 1;
            }
            return Ok(());
        }

        self.render_console(
            renderer_state,
            renderer,
            console,
            scheduler,
            overlay_lines,
            Some(command_prompt),
            Some(sidebar),
        )
    }

    fn apply_host_action(
        &mut self,
        action: ConsoleAction,
        runtime: &AppConfig,
        size: crate::terminal::TerminalSize,
        live_surface: &mut LiveSurfaceState,
        hosted: &mut HashMap<SessionAddress, HostedSession>,
        tx: &Sender<RuntimeEvent>,
        console: &mut ConsoleState,
        scheduler: &mut SchedulerState,
        renderer_state: &mut RendererState,
        renderer: &Renderer,
        command_prompt: &mut CommandPromptState,
        sidebar: &mut SidebarState,
    ) -> Result<bool, AppError> {
        let active_addresses = self.active_session_addresses();
        let changed = match action {
            ConsoleAction::CreateSession => {
                let address = self.spawn_default_shell_session(
                    &runtime.node.node_id,
                    size,
                    sidebar.hidden,
                    hosted,
                    tx,
                )?;
                console.focus(address);
                command_prompt.set_message("Created new session.");
                true
            }
            ConsoleAction::ListSessions => {
                command_prompt
                    .toggle_sessions(&self.sessions.list(), console.focused_session.as_ref());
                true
            }
            ConsoleAction::EnterNativeFullscreen => false,
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
            ConsoleAction::FocusAddress(ref address) => {
                let changed = console.focus_address(&active_addresses, address).is_some();
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
            self.refresh_surface(
                RenderSurface::Server,
                live_surface,
                hosted,
                renderer_state,
                renderer,
                console,
                scheduler,
                command_prompt,
                sidebar,
            )?;
        }

        Ok(false)
    }

    fn apply_workspace_action(
        &mut self,
        action: ConsoleAction,
        runtime: &AppConfig,
        size: crate::terminal::TerminalSize,
        live_surface: &mut LiveSurfaceState,
        hosted: &mut HashMap<SessionAddress, HostedSession>,
        tx: &Sender<RuntimeEvent>,
        console: &mut ConsoleState,
        scheduler: &mut SchedulerState,
        renderer_state: &mut RendererState,
        renderer: &Renderer,
        command_prompt: &mut CommandPromptState,
        sidebar: &mut SidebarState,
    ) -> Result<bool, AppError> {
        let active_addresses = self.active_session_addresses();
        let changed = match action {
            ConsoleAction::CreateSession => {
                let address = self.spawn_default_shell_session(
                    &runtime.node.node_id,
                    size,
                    sidebar.hidden,
                    hosted,
                    tx,
                )?;
                console.focus(address);
                command_prompt.set_message("Created new session.");
                true
            }
            ConsoleAction::ListSessions => {
                command_prompt
                    .toggle_sessions(&self.sessions.list(), console.focused_session.as_ref());
                true
            }
            ConsoleAction::EnterNativeFullscreen => false,
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
            ConsoleAction::FocusAddress(ref address) => {
                let changed = console.focus_address(&active_addresses, address).is_some();
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
            self.refresh_surface(
                RenderSurface::Workspace,
                live_surface,
                hosted,
                renderer_state,
                renderer,
                console,
                scheduler,
                command_prompt,
                sidebar,
            )?;
        }

        Ok(false)
    }

    fn apply_command_outcome(
        &mut self,
        outcome: CommandPromptOutcome,
        runtime: &AppConfig,
        size: crate::terminal::TerminalSize,
        native_fullscreen: &mut NativeFullscreenState,
        alternate_screen: &mut AlternateScreenGuard,
        live_surface: &mut LiveSurfaceState,
        hosted: &mut HashMap<SessionAddress, HostedSession>,
        tx: &Sender<RuntimeEvent>,
        console: &mut ConsoleState,
        scheduler: &mut SchedulerState,
        renderer_state: &mut RendererState,
        renderer: &Renderer,
        command_prompt: &mut CommandPromptState,
        sidebar: &mut SidebarState,
        surface: RenderSurface,
    ) -> Result<bool, AppError> {
        match outcome {
            CommandPromptOutcome::RenderOnly => {
                self.refresh_surface(
                    surface,
                    live_surface,
                    hosted,
                    renderer_state,
                    renderer,
                    console,
                    scheduler,
                    command_prompt,
                    sidebar,
                )?;
                Ok(false)
            }
            CommandPromptOutcome::Execute(command) => self.execute_command_prompt(
                command.as_str(),
                runtime,
                size,
                native_fullscreen,
                alternate_screen,
                live_surface,
                hosted,
                tx,
                console,
                scheduler,
                renderer_state,
                renderer,
                command_prompt,
                sidebar,
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
        native_fullscreen: &mut NativeFullscreenState,
        alternate_screen: &mut AlternateScreenGuard,
        live_surface: &mut LiveSurfaceState,
        hosted: &mut HashMap<SessionAddress, HostedSession>,
        tx: &Sender<RuntimeEvent>,
        console: &mut ConsoleState,
        scheduler: &mut SchedulerState,
        renderer_state: &mut RendererState,
        renderer: &Renderer,
        command_prompt: &mut CommandPromptState,
        sidebar: &mut SidebarState,
        surface: RenderSurface,
    ) -> Result<bool, AppError> {
        let trimmed = command.trim();
        let mut should_exit = false;
        let mut refresh_surface_after_command = true;

        if trimmed.is_empty() {
            command_prompt.set_message("Empty command. Try /h.");
        } else if matches!(trimmed, "/h" | "/help") {
            command_prompt.toggle_help();
        } else if trimmed == "/new" {
            let address = self.spawn_default_shell_session(
                &runtime.node.node_id,
                size,
                sidebar.hidden,
                hosted,
                tx,
            )?;
            console.focus(address.clone());
            scheduler.on_manual_switch(console);
            command_prompt.set_message(format!("Created {address}."));
        } else if matches!(trimmed, "/sessions" | "/ls") {
            command_prompt.toggle_sessions(&self.sessions.list(), console.focused_session.as_ref());
        } else if matches!(trimmed, "/fullscreen" | "/full" | "/f") {
            if self.enter_native_fullscreen(
                native_fullscreen,
                alternate_screen,
                live_surface,
                hosted,
                console,
            )? {
                command_prompt.clear_overlay();
                refresh_surface_after_command = false;
            } else {
                command_prompt.set_message("Fullscreen unavailable for current focus.");
            }
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

        if refresh_surface_after_command {
            self.refresh_surface(
                surface,
                live_surface,
                hosted,
                renderer_state,
                renderer,
                console,
                scheduler,
                command_prompt,
                sidebar,
            )?;
        }
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
        sidebar: &mut SidebarState,
    ) -> Result<(), AppError> {
        match surface {
            RenderSurface::Workspace => self.render_workspace_console(
                renderer_state,
                renderer,
                console,
                scheduler,
                command_prompt,
                sidebar,
            ),
            RenderSurface::Server => self.render_host_console(
                renderer_state,
                renderer,
                console,
                scheduler,
                command_prompt,
                sidebar,
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
        sidebar: Option<&mut SidebarState>,
        console: &ConsoleState,
        scheduler: &SchedulerState,
        command_prompt: Option<&CommandPromptState>,
    ) -> (String, Option<SidebarOverlay>) {
        let mut lines = workspace_idle_lines(surface, active_count, waiting_count);
        let target_rows = self.terminal.current_size_or_default().rows as usize;
        let show_footer_menu = !overlay_lines.is_empty();
        let reserved_rows = lines.len() + overlay_lines.len() + 1 + usize::from(show_footer_menu);
        let spacer_rows = target_rows.saturating_sub(reserved_rows);
        lines.extend(std::iter::repeat(String::new()).take(spacer_rows));
        if show_footer_menu {
            lines.push(style_footer_separator_line(
                self.terminal.current_size_or_default().cols as usize,
            ));
        }
        lines.extend(overlay_lines.iter().map(|line| {
            style_overlay_line(line, self.terminal.current_size_or_default().cols as usize)
        }));
        let status_line = bottom_line.to_string();
        let sidebar_state =
            self.build_sidebar_render_state(sidebar, console, scheduler, command_prompt);
        lines.push(style_status_line(
            &status_line,
            self.terminal.current_size_or_default().cols as usize,
        ));
        let sidebar_overlay = self.build_sidebar_overlay(sidebar_state.as_ref());
        (lines.join("\r\n"), sidebar_overlay)
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
    transcript: TerminalTranscript,
    input_normalizer: ForwardInputNormalizer,
    command_tracker: ShellCommandTracker,
    viewport_size: crate::terminal::TerminalSize,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct LiveSurfaceState {
    active: bool,
    session: Option<SessionAddress>,
    bootstrapping_sessions: HashMap<SessionAddress, u128>,
    known_live_command_sessions: HashSet<SessionAddress>,
    chrome_visible: bool,
    overlay_rows: usize,
    sidebar_overlay: Option<SidebarOverlay>,
    separator_line: String,
    keys_line: String,
    status_line: String,
    pending_redraw: bool,
    pending_sync_marker_bytes: Vec<u8>,
    pending_agent_output_batch: Vec<u8>,
    agent_sync_batch_open: bool,
}

impl LiveSurfaceState {
    fn display_may_be_live_owned(&self) -> bool {
        self.session.is_some()
            && (self.active
                || self.chrome_visible
                || self.overlay_rows > 0
                || self.sidebar_overlay.is_some()
                || self.pending_redraw)
    }

    fn is_live_for(&self, session: &SessionAddress) -> bool {
        self.active && self.session.as_ref() == Some(session)
    }

    fn owns_display(&self, session: &SessionAddress, now_unix_ms: u128) -> bool {
        self.session.as_ref() == Some(session)
            && (self.active || self.is_bootstrapping(session, now_unix_ms))
    }

    fn mark_session_bootstrapping(&mut self, session: SessionAddress, now_unix_ms: u128) {
        self.bootstrapping_sessions.insert(session, now_unix_ms);
    }

    fn mark_known_live_command(&mut self, session: SessionAddress) {
        self.known_live_command_sessions.insert(session);
    }

    fn clear_known_live_command(&mut self, session: &SessionAddress) {
        self.known_live_command_sessions.remove(session);
    }

    fn clear_session_bootstrapping(&mut self, session: &SessionAddress) {
        self.bootstrapping_sessions.remove(session);
    }

    fn is_known_live_command(&self, session: &SessionAddress) -> bool {
        self.known_live_command_sessions.contains(session)
    }

    fn is_bootstrapping(&self, session: &SessionAddress, now_unix_ms: u128) -> bool {
        self.bootstrapping_sessions
            .get(session)
            .map(|started_at| now_unix_ms.saturating_sub(*started_at) < 5_000)
            .unwrap_or(false)
    }

    fn set_display_session(
        &mut self,
        session: Option<SessionAddress>,
        active: bool,
        _now_unix_ms: u128,
    ) {
        if session.is_some() && (self.session != session || (active && !self.active)) {
            self.pending_redraw = session.is_some();
        }
        self.active = active && session.is_some();
        self.session = session;
        if !self.active {
            self.chrome_visible = false;
            self.overlay_rows = 0;
            self.sidebar_overlay = None;
            self.separator_line.clear();
            self.keys_line.clear();
            self.status_line.clear();
            self.pending_sync_marker_bytes.clear();
            self.pending_agent_output_batch.clear();
            self.agent_sync_batch_open = false;
        }
        if self.session.is_none() {
            self.chrome_visible = false;
            self.overlay_rows = 0;
            self.sidebar_overlay = None;
            self.separator_line.clear();
            self.keys_line.clear();
            self.status_line.clear();
            self.pending_redraw = false;
            self.pending_sync_marker_bytes.clear();
            self.pending_agent_output_batch.clear();
            self.agent_sync_batch_open = false;
        }
    }

    #[cfg(test)]
    fn begin_passthrough_output(&mut self) -> bool {
        let needs_redraw = self.chrome_visible || self.overlay_rows > 0;
        self.chrome_visible = false;
        self.overlay_rows = 0;
        self.sidebar_overlay = None;
        self.separator_line.clear();
        self.keys_line.clear();
        self.status_line.clear();
        self.pending_sync_marker_bytes.clear();
        self.pending_agent_output_batch.clear();
        self.agent_sync_batch_open = false;
        if needs_redraw {
            self.pending_redraw = true;
        }
        needs_redraw
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct NativeFullscreenState {
    session: Option<SessionAddress>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NativeFullscreenInputOutcome {
    forwarded: Vec<u8>,
    exit_requested: bool,
}

impl NativeFullscreenState {
    fn is_active(&self) -> bool {
        self.session.is_some()
    }

    fn session(&self) -> Option<&SessionAddress> {
        self.session.as_ref()
    }

    fn is_active_for(&self, session: &SessionAddress) -> bool {
        self.session.as_ref() == Some(session)
    }

    fn activate(&mut self, session: SessionAddress) {
        self.session = Some(session);
    }

    fn deactivate(&mut self) -> Option<SessionAddress> {
        self.session.take()
    }

    fn handle_input(&mut self, bytes: &[u8]) -> NativeFullscreenInputOutcome {
        if shortcut_matches(bytes, SHORTCUT_FULLSCREEN) {
            return NativeFullscreenInputOutcome {
                forwarded: Vec::new(),
                exit_requested: true,
            };
        }

        let mut forwarded = Vec::new();
        let mut exit_requested = false;

        for &byte in bytes {
            if byte == SHORTCUT_FULLSCREEN {
                exit_requested = true;
                break;
            } else {
                forwarded.push(byte);
            }
        }

        NativeFullscreenInputOutcome {
            forwarded,
            exit_requested,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CursorPlacement {
    row: u16,
    col: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SidebarOverlay {
    separator_col: usize,
    content_col: usize,
    divider: String,
    lines: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LiveSurfaceUiBuffer {
    buffer: String,
    overlay_rows: usize,
    sidebar_overlay: Option<SidebarOverlay>,
    separator_line: String,
    keys_line: String,
    status_line: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RenderSurface {
    Workspace,
    Server,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ConsoleAction {
    CreateSession,
    ListSessions,
    CloseCurrentSession,
    EnterNativeFullscreen,
    DismissOverlay,
    NextSession,
    PreviousSession,
    FocusIndex(usize),
    FocusAddress(SessionAddress),
    TogglePeek,
    QuitHost,
}

const COMMAND_BAR_PREFIX: u8 = 0x17;
const COMMAND_BAR_PREFIX_FALLBACK: u8 = 0x07;
const SHORTCUT_PREVIOUS_SESSION: u8 = 0x02;
const SHORTCUT_NEXT_SESSION: u8 = 0x06;
const SHORTCUT_NEW_SESSION: u8 = 0x0e;
const SHORTCUT_FULLSCREEN: u8 = SHORTCUT_NATIVE_FULLSCREEN;
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
        if !self.open
            && (shortcut_matches(bytes, COMMAND_BAR_PREFIX)
                || shortcut_matches(bytes, COMMAND_BAR_PREFIX_FALLBACK))
        {
            self.open = true;
            self.buffer.clear();
            self.clear_pending_picker_escape();
            return Some(CommandPromptOutcome::RenderOnly);
        }

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

    fn set_passive_message(&mut self, message: impl Into<String>) {
        if self.open || self.has_blocking_overlay() {
            return;
        }

        self.overlay = CommandOverlay::Message(message.into());
        self.clear_pending_picker_escape();
    }

    fn clear_overlay(&mut self) {
        self.overlay = CommandOverlay::None;
        self.picker_selection = None;
        self.clear_pending_picker_escape();
    }

    fn has_blocking_overlay(&self) -> bool {
        matches!(
            self.overlay,
            CommandOverlay::Help | CommandOverlay::Sessions
        )
    }

    fn wants_escape_dismiss(&self) -> bool {
        self.open || self.has_blocking_overlay()
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
        if self.open || !self.has_blocking_overlay() {
            return false;
        }

        matches_overlay_submit(bytes)
    }

    fn clear_message_on_forwarded_input(&mut self, bytes: &[u8]) -> bool {
        if !matches!(self.overlay, CommandOverlay::Message(_)) || bytes.is_empty() {
            return false;
        }

        self.clear_overlay();
        true
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

        match picker_escape_action(&combined) {
            Some(PickerEscapeAction::Previous) => {
                self.clear_pending_picker_escape();
                let moved = self.move_picker_previous(sessions, focused);
                Some(if moved {
                    PickerNavigationOutcome::Render
                } else {
                    PickerNavigationOutcome::Consumed
                })
            }
            Some(PickerEscapeAction::Next) => {
                self.clear_pending_picker_escape();
                let moved = self.move_picker_next(sessions, focused);
                Some(if moved {
                    PickerNavigationOutcome::Render
                } else {
                    PickerNavigationOutcome::Consumed
                })
            }
            Some(PickerEscapeAction::Submit) => {
                self.clear_pending_picker_escape();
                Some(PickerNavigationOutcome::Submit)
            }
            None if combined.first() == Some(&0x1b)
                && consume_input_escape(&combined, 0).is_none() =>
            {
                self.pending_picker_escape = combined;
                self.pending_picker_started_at_unix_ms = Some(now_unix_ms);
                Some(PickerNavigationOutcome::Consumed)
            }
            None => {
                self.clear_pending_picker_escape();
                None
            }
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
            CommandOverlay::Message(_) => {}
            CommandOverlay::Help => {
                lines.push(
                    "help: /new /sessions /focus <n|id> /fullscreen /close /quit /clear"
                        .to_string(),
                );
                lines.push(
                    "help: Esc hide | Ctrl-B prev | Ctrl-F next | Ctrl-L picker | Ctrl-N new"
                        .to_string(),
                );
                lines.push("help: Ctrl-O enter/exit fullscreen | /fullscreen".to_string());
            }
            CommandOverlay::Sessions => {
                lines.push(
                    "sessions: Up/Down move  ^B prev  ^F next  Enter select  Esc close  1-9 direct"
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
                        "{} {:>2}. {} | {} | cwd: {}",
                        marker,
                        index + 1,
                        session.address(),
                        session.title,
                        picker_session_cwd(session)
                    ));
                }
            }
        }

        lines.push(
            "keys: ^W cmd  ^B/^F switch  ^N new  ^O full on/off  ^L picker  ^X close  ^Q quit"
                .to_string(),
        );

        if self.open {
            lines.push(format!(":{}", self.buffer));
        }

        lines
    }

    fn status_line(&self, default: impl Into<String>) -> String {
        match &self.overlay {
            CommandOverlay::Message(message) => format!("notice: {message}"),
            _ => default.into(),
        }
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

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct SidebarState {
    focused: bool,
    hidden: bool,
    selection: Option<SessionAddress>,
    rendered_overlay: Option<SidebarOverlay>,
    rendered_cursor_visible: bool,
    force_full_redraws: u8,
    pending_navigation_escape: Vec<u8>,
    pending_navigation_started_at_unix_ms: Option<u128>,
    recovering_navigation_escape: bool,
}

impl SidebarState {
    fn rendered(&self) -> bool {
        true
    }

    fn modal_active(&self, command_prompt: &CommandPromptState) -> bool {
        self.focused
            && !self.hidden
            && !command_prompt.open
            && !command_prompt.has_blocking_overlay()
    }

    fn sync_selection(
        &mut self,
        sessions: &[&crate::session::SessionRecord],
        focused: Option<&SessionAddress>,
    ) {
        let active = picker_sessions(sessions);
        if active.is_empty() {
            self.selection = None;
            return;
        }

        if self
            .selection
            .as_ref()
            .map(|selected| active.iter().any(|session| session.address() == selected))
            .unwrap_or(false)
        {
            return;
        }

        self.selection = focused
            .filter(|target| active.iter().any(|session| session.address() == *target))
            .cloned()
            .or_else(|| active.first().map(|session| session.address().clone()));
    }

    fn handle_navigation(
        &mut self,
        bytes: &[u8],
        sessions: &[&crate::session::SessionRecord],
        focused: Option<&SessionAddress>,
        allow_entry: bool,
        command_prompt: &CommandPromptState,
        now_unix_ms: u128,
    ) -> Option<SidebarNavigationOutcome> {
        if self.recovering_navigation_escape {
            let mut combined = self.pending_navigation_escape.clone();
            combined.extend_from_slice(bytes);
            match sidebar_escape_action(&combined) {
                Some(SidebarEscapeAction::Close) => {
                    self.clear_pending_navigation_escape();
                    return Some(SidebarNavigationOutcome::Consumed);
                }
                None if combined.first() == Some(&0x1b)
                    && consume_input_escape(&combined, 0).is_none() =>
                {
                    self.pending_navigation_escape = combined;
                    self.pending_navigation_started_at_unix_ms = Some(now_unix_ms);
                    return Some(SidebarNavigationOutcome::Consumed);
                }
                _ => {
                    self.clear_pending_navigation_escape();
                }
            }
        }

        if self.hidden {
            self.clear_pending_navigation_escape();
            if !allow_entry || command_prompt.open || command_prompt.has_blocking_overlay() {
                return None;
            }

            return match sidebar_escape_action(bytes) {
                Some(SidebarEscapeAction::Open | SidebarEscapeAction::Close) => {
                    self.hidden = false;
                    self.focused = true;
                    self.sync_selection(sessions, focused);
                    Some(SidebarNavigationOutcome::Render)
                }
                _ => None,
            };
        }

        if !self.focused {
            self.clear_pending_navigation_escape();
            if !allow_entry || command_prompt.open || command_prompt.has_blocking_overlay() {
                return None;
            }

            return match sidebar_escape_action(bytes) {
                Some(SidebarEscapeAction::Open) => {
                    self.focused = true;
                    self.sync_selection(sessions, focused);
                    Some(SidebarNavigationOutcome::Render)
                }
                _ => None,
            };
        }

        if !self.modal_active(command_prompt) {
            self.clear_pending_navigation_escape();
            return None;
        }

        let mut combined = self.pending_navigation_escape.clone();
        combined.extend_from_slice(bytes);

        match sidebar_escape_action(&combined) {
            Some(SidebarEscapeAction::Open) => {
                self.clear_pending_navigation_escape();
                Some(SidebarNavigationOutcome::Consumed)
            }
            Some(SidebarEscapeAction::Close) => {
                self.clear_pending_navigation_escape();
                self.focused = false;
                Some(SidebarNavigationOutcome::Render)
            }
            Some(SidebarEscapeAction::Hide) => {
                self.clear_pending_navigation_escape();
                self.focused = false;
                self.hidden = true;
                Some(SidebarNavigationOutcome::Render)
            }
            Some(SidebarEscapeAction::Previous) => {
                self.clear_pending_navigation_escape();
                let moved = self.move_selection(sessions, focused, -1);
                Some(if moved {
                    SidebarNavigationOutcome::Render
                } else {
                    SidebarNavigationOutcome::Consumed
                })
            }
            Some(SidebarEscapeAction::Next) => {
                self.clear_pending_navigation_escape();
                let moved = self.move_selection(sessions, focused, 1);
                Some(if moved {
                    SidebarNavigationOutcome::Render
                } else {
                    SidebarNavigationOutcome::Consumed
                })
            }
            Some(SidebarEscapeAction::Submit) => {
                self.clear_pending_navigation_escape();
                self.focused = false;
                self.sync_selection(sessions, focused);
                self.selection
                    .clone()
                    .map(SidebarNavigationOutcome::Submit)
                    .or(Some(SidebarNavigationOutcome::Consumed))
            }
            None if combined.first() == Some(&0x1b)
                && consume_input_escape(&combined, 0).is_none() =>
            {
                self.pending_navigation_escape = combined;
                self.pending_navigation_started_at_unix_ms = Some(now_unix_ms);
                Some(SidebarNavigationOutcome::Consumed)
            }
            None => {
                self.clear_pending_navigation_escape();
                None
            }
        }
    }

    fn flush_navigation_timeout(
        &mut self,
        command_prompt: &CommandPromptState,
        now_unix_ms: u128,
    ) -> bool {
        if !self.modal_active(command_prompt) {
            if self.recovering_navigation_escape {
                if let Some(started_at) = self.pending_navigation_started_at_unix_ms {
                    if now_unix_ms.saturating_sub(started_at) < SIDEBAR_NAVIGATION_TIMEOUT_MS {
                        return false;
                    }
                }
            }
            self.clear_pending_navigation_escape();
            return false;
        }

        let Some(started_at) = self.pending_navigation_started_at_unix_ms else {
            return false;
        };

        if now_unix_ms.saturating_sub(started_at) < SIDEBAR_NAVIGATION_TIMEOUT_MS {
            return false;
        }

        let pending = self.pending_navigation_escape.clone();
        if pending == [0x1b] {
            self.focused = false;
            self.recovering_navigation_escape = true;
            self.pending_navigation_escape = pending;
            self.pending_navigation_started_at_unix_ms = Some(now_unix_ms);
            true
        } else {
            self.clear_pending_navigation_escape();
            false
        }
    }

    fn selected_session(
        &mut self,
        sessions: &[&crate::session::SessionRecord],
        focused: Option<&SessionAddress>,
    ) -> Option<SessionAddress> {
        self.sync_selection(sessions, focused);
        self.selection.clone()
    }

    fn move_selection(
        &mut self,
        sessions: &[&crate::session::SessionRecord],
        focused: Option<&SessionAddress>,
        delta: isize,
    ) -> bool {
        let active = picker_sessions(sessions);
        if active.is_empty() {
            self.selection = None;
            return false;
        }

        self.sync_selection(sessions, focused);
        let current = self
            .selection
            .as_ref()
            .and_then(|selected| {
                active
                    .iter()
                    .position(|session| session.address() == selected)
            })
            .unwrap_or(0);
        let len = active.len() as isize;
        let next = ((current as isize + delta).rem_euclid(len)) as usize;
        self.selection = Some(active[next].address().clone());
        true
    }

    fn clear_pending_navigation_escape(&mut self) {
        self.pending_navigation_escape.clear();
        self.pending_navigation_started_at_unix_ms = None;
        self.recovering_navigation_escape = false;
    }
}

fn live_overlay_lines(
    command_prompt: &CommandPromptState,
    sessions: Vec<&crate::session::SessionRecord>,
    focused: Option<&SessionAddress>,
) -> Vec<String> {
    command_prompt
        .overlay_lines(sessions, focused)
        .into_iter()
        .filter(|line| !line.starts_with("keys:"))
        .collect()
}

fn picker_session_cwd(session: &crate::session::SessionRecord) -> &str {
    session.current_working_dir.as_deref().unwrap_or("unknown")
}

fn live_overlay_visible(command_prompt: &CommandPromptState, sidebar: &SidebarState) -> bool {
    command_prompt.open
        || command_prompt.has_blocking_overlay()
        || sidebar.modal_active(command_prompt)
}

#[derive(Debug, Default)]
struct ShellCommandTracker {
    buffer: String,
    pending_escape: Vec<u8>,
}

impl ShellCommandTracker {
    fn observe(&mut self, bytes: &[u8]) -> Option<String> {
        let mut input = Vec::with_capacity(self.pending_escape.len() + bytes.len());
        input.extend_from_slice(&self.pending_escape);
        input.extend_from_slice(bytes);
        self.pending_escape.clear();

        let mut submitted = None;
        let mut index = 0;
        while index < input.len() {
            match input[index] {
                b'\r' | b'\n' => {
                    let command = self.buffer.trim().to_string();
                    self.buffer.clear();
                    if !command.is_empty() {
                        submitted = Some(command);
                    }
                    index += 1;
                }
                0x08 | 0x7f => {
                    self.buffer.pop();
                    index += 1;
                }
                0x03 | 0x04 => {
                    self.buffer.clear();
                    index += 1;
                }
                0x1b => match consume_input_escape(&input, index) {
                    Some(next_index) => {
                        if matches_submit_escape(&input[index..next_index]) {
                            let command = self.buffer.trim().to_string();
                            self.buffer.clear();
                            if !command.is_empty() {
                                submitted = Some(command);
                            }
                        }
                        index = next_index;
                    }
                    None => {
                        self.pending_escape.extend_from_slice(&input[index..]);
                        break;
                    }
                },
                byte if (0x20..=0x7e).contains(&byte) => {
                    self.buffer.push(byte as char);
                    index += 1;
                }
                _ => {
                    index += 1;
                }
            }
        }

        submitted
    }

    fn pending_live_command_label(&self) -> Option<String> {
        live_command_label(self.buffer.trim())
    }
}

fn bytes_include_submit(bytes: &[u8]) -> bool {
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'\r' | b'\n' => return true,
            0x1b => match consume_input_escape(bytes, index) {
                Some(next_index) => {
                    if matches_submit_escape(&bytes[index..next_index]) {
                        return true;
                    }
                    index = next_index;
                }
                None => break,
            },
            _ => index += 1,
        }
    }
    false
}

fn live_command_label_from_shell_snapshot(
    snapshot: &crate::terminal::ScreenSnapshot,
) -> Option<String> {
    if snapshot.alternate_screen {
        return None;
    }

    let line = snapshot.lines.get(snapshot.cursor_row as usize)?;
    let visible_prefix = line
        .chars()
        .take(snapshot.cursor_col as usize)
        .collect::<String>();
    let submitted_command = extract_shell_prompt_command(&visible_prefix)?;
    live_command_label(&submitted_command)
}

fn extract_shell_prompt_command(line: &str) -> Option<String> {
    let trimmed = line.trim_end();
    if trimmed.is_empty() {
        return None;
    }

    ["$ ", "# ", "% ", "> ", ": "]
        .iter()
        .filter_map(|marker| {
            trimmed
                .rsplit_once(marker)
                .map(|(_, command)| command.trim())
        })
        .find(|command| !command.is_empty())
        .map(ToString::to_string)
}

fn native_fullscreen_seed_snapshot(
    transcript: &TerminalTranscript,
    terminal_size: crate::terminal::TerminalSize,
) -> crate::terminal::ScreenSnapshot {
    transcript.replay_active_snapshot(terminal_size)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PickerNavigationOutcome {
    Consumed,
    Render,
    Submit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SidebarRenderState {
    collapsed: bool,
    focused: bool,
    width: usize,
    lines: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SidebarNavigationOutcome {
    Consumed,
    Render,
    Submit(SessionAddress),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SidebarEscapeAction {
    Open,
    Close,
    Hide,
    Previous,
    Next,
    Submit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PickerEscapeAction {
    Previous,
    Next,
    Submit,
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

fn matches_overlay_submit(bytes: &[u8]) -> bool {
    matches!(bytes, b"\r" | b"\n" | b"\r\n")
        || matches!(
            picker_escape_action(bytes),
            Some(PickerEscapeAction::Submit)
        )
}

fn picker_escape_action(bytes: &[u8]) -> Option<PickerEscapeAction> {
    match bytes {
        b"\x1b[A" | b"\x1bOA" => Some(PickerEscapeAction::Previous),
        b"\x1b[B" | b"\x1bOB" => Some(PickerEscapeAction::Next),
        b"\x1bOM" => Some(PickerEscapeAction::Submit),
        _ if matches_submit_escape(bytes) => Some(PickerEscapeAction::Submit),
        _ => None,
    }
}

fn sidebar_escape_action(bytes: &[u8]) -> Option<SidebarEscapeAction> {
    match bytes {
        b"\x1b[C" | b"\x1bOC" => Some(SidebarEscapeAction::Open),
        b"\x1b[D" | b"\x1bOD" => Some(SidebarEscapeAction::Close),
        b"h" | b"H" => Some(SidebarEscapeAction::Hide),
        b"\x1b[A" | b"\x1bOA" => Some(SidebarEscapeAction::Previous),
        b"\x1b[B" | b"\x1bOB" => Some(SidebarEscapeAction::Next),
        _ if matches_overlay_submit(bytes) => Some(SidebarEscapeAction::Submit),
        _ => None,
    }
}

fn matches_kitty_enter(bytes: &[u8]) -> bool {
    let Some(payload) = bytes.strip_prefix(b"\x1b[") else {
        return false;
    };
    let Some(payload) = payload.strip_suffix(b"u") else {
        return false;
    };
    let mut parts = payload.split(|byte| *byte == b';');
    matches!(parts.next(), Some(b"13"))
        && parts.all(|part| !part.is_empty() && part.iter().all(|byte| byte.is_ascii_digit()))
}

fn matches_submit_escape(bytes: &[u8]) -> bool {
    bytes == b"\x1bOM" || matches_kitty_enter(bytes) || matches_csi_tilde_enter(bytes)
}

fn matches_csi_tilde_enter(bytes: &[u8]) -> bool {
    let Some(payload) = bytes.strip_prefix(b"\x1b[") else {
        return false;
    };
    let Some(payload) = payload.strip_suffix(b"~") else {
        return false;
    };
    let mut saw_digit = false;
    let mut last_value = 0_u32;
    let mut current_value = 0_u32;
    let mut in_value = false;

    for &byte in payload {
        match byte {
            b'0'..=b'9' => {
                in_value = true;
                current_value = current_value
                    .saturating_mul(10)
                    .saturating_add(u32::from(byte - b'0'));
            }
            b';' => {
                if !in_value {
                    return false;
                }
                saw_digit = true;
                last_value = current_value;
                current_value = 0;
                in_value = false;
            }
            _ => return false,
        }
    }

    if !in_value && !saw_digit {
        return false;
    }

    let final_value = if in_value { current_value } else { last_value };
    final_value == 13
}

#[derive(Debug, Default)]
struct InputTracker {
    pending_bytes: usize,
    pending_escape: Vec<u8>,
}

impl InputTracker {
    fn observe(
        &mut self,
        bytes: &[u8],
        console: &mut ConsoleState,
        scheduler: &mut SchedulerState,
        now_unix_ms: u128,
    ) {
        let mut input = Vec::with_capacity(self.pending_escape.len() + bytes.len());
        input.extend_from_slice(&self.pending_escape);
        input.extend_from_slice(bytes);
        self.pending_escape.clear();

        let mut index = 0;
        while index < input.len() {
            match input[index] {
                b'\r' | b'\n' => {
                    let submitted_input_bytes = self.pending_bytes;
                    self.pending_bytes = 0;
                    scheduler.on_input_submitted_with_bytes(
                        console,
                        now_unix_ms,
                        submitted_input_bytes,
                    );
                    index += 1;
                }
                0x08 | 0x7f => {
                    self.pending_bytes = self.pending_bytes.saturating_sub(1);
                    if self.pending_bytes == 0 {
                        console.clear_input();
                    } else {
                        console.start_typing();
                        console.set_input_len(self.pending_bytes);
                    }
                    index += 1;
                }
                b'\t' => {
                    index += 1;
                }
                0x1b => match consume_input_escape(&input, index) {
                    Some(next_index) => index = next_index,
                    None => {
                        self.pending_escape.extend_from_slice(&input[index..]);
                        break;
                    }
                },
                byte if is_typing_byte(byte) => {
                    self.pending_bytes += 1;
                    console.start_typing();
                    console.set_input_len(self.pending_bytes);
                    index += 1;
                }
                _ => {
                    index += 1;
                }
            }
        }
    }
}

#[derive(Debug, Default)]
struct ForwardInputNormalizer {
    pending_escape: Vec<u8>,
    pending_escape_started_at_unix_ms: Option<u128>,
}

impl ForwardInputNormalizer {
    fn normalize(
        &mut self,
        bytes: &[u8],
        application_cursor_keys: bool,
        now_unix_ms: u128,
    ) -> Vec<u8> {
        let mut input = Vec::with_capacity(self.pending_escape.len() + bytes.len());
        input.extend_from_slice(&self.pending_escape);
        input.extend_from_slice(bytes);
        self.pending_escape.clear();
        self.pending_escape_started_at_unix_ms = None;

        let mut output = Vec::with_capacity(input.len());
        let mut index = 0;
        while index < input.len() {
            if input[index] != 0x1b {
                output.push(input[index]);
                index += 1;
                continue;
            }

            if index + 1 >= input.len() {
                self.pending_escape.extend_from_slice(&input[index..]);
                self.pending_escape_started_at_unix_ms = Some(now_unix_ms);
                break;
            }

            match input[index + 1] {
                b'[' => {
                    if index + 2 >= input.len() {
                        self.pending_escape.extend_from_slice(&input[index..]);
                        self.pending_escape_started_at_unix_ms = Some(now_unix_ms);
                        break;
                    }

                    let final_byte = input[index + 2];
                    if application_cursor_keys
                        && matches!(final_byte, b'A' | b'B' | b'C' | b'D' | b'H' | b'F')
                    {
                        output.extend_from_slice(&[0x1b, b'O', final_byte]);
                    } else {
                        output.extend_from_slice(&input[index..index + 3]);
                    }
                    index += 3;
                }
                b'O' => {
                    if index + 2 >= input.len() {
                        self.pending_escape.extend_from_slice(&input[index..]);
                        self.pending_escape_started_at_unix_ms = Some(now_unix_ms);
                        break;
                    }

                    output.extend_from_slice(&input[index..index + 3]);
                    index += 3;
                }
                _ => {
                    output.extend_from_slice(&input[index..index + 2]);
                    index += 2;
                }
            }
        }

        output
    }

    fn flush_pending_escape_timeout(&mut self, now_unix_ms: u128) -> Vec<u8> {
        let Some(started_at) = self.pending_escape_started_at_unix_ms else {
            return Vec::new();
        };

        if now_unix_ms.saturating_sub(started_at) < PICKER_ESCAPE_TIMEOUT_MS {
            return Vec::new();
        }

        self.pending_escape_started_at_unix_ms = None;
        std::mem::take(&mut self.pending_escape)
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

fn runtime_event_is_input_priority(event: &RuntimeEvent) -> bool {
    matches!(event, RuntimeEvent::Input(_) | RuntimeEvent::InputClosed)
}

fn buffer_pending_runtime_events(
    rx: &Receiver<RuntimeEvent>,
    pending_events: &mut VecDeque<RuntimeEvent>,
) {
    loop {
        match rx.try_recv() {
            Ok(event) => pending_events.push_back(event),
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
        }
    }
}

fn next_runtime_event(
    rx: &Receiver<RuntimeEvent>,
    pending_events: &mut VecDeque<RuntimeEvent>,
) -> Result<RuntimeEvent, RecvTimeoutError> {
    if let Some(index) = pending_events
        .iter()
        .position(runtime_event_is_input_priority)
    {
        return Ok(pending_events
            .remove(index)
            .expect("pending input event index must exist"));
    }

    if let Some(event) = pending_events.pop_front() {
        return Ok(event);
    }

    rx.recv_timeout(EVENT_LOOP_TICK)
}

fn workspace_idle_lines(surface: &str, active_count: usize, waiting_count: usize) -> Vec<String> {
    vec![
        format!("WaitAgent | {surface}"),
        format!("active: {active_count} | waiting: {waiting_count}"),
        "hint: Ctrl-W command bar | Ctrl-B/Ctrl-F switch | Ctrl-O fullscreen on/off | Ctrl-Q quit"
            .to_string(),
    ]
}

fn background_wait_notice(
    previous_waiting: &[SessionAddress],
    current_waiting: &[SessionAddress],
    focused: Option<&SessionAddress>,
) -> Option<String> {
    let new_waiters = current_waiting
        .iter()
        .filter(|address| !previous_waiting.contains(address))
        .filter(|address| Some(*address) != focused)
        .collect::<Vec<_>>();

    match new_waiters.as_slice() {
        [] => None,
        [single] => Some(format!("{single} is waiting. Press Enter to hand off.")),
        many => Some(format!(
            "{} background sessions are waiting. Press Enter to hand off.",
            many.len()
        )),
    }
}

fn focused_passthrough_output(
    live_surface: &LiveSurfaceState,
    console: &ConsoleState,
    command_prompt: &CommandPromptState,
    sidebar: &SidebarState,
    output_session: &SessionAddress,
) -> bool {
    console.focused_session.as_ref() == Some(output_session)
        && !console.is_peeking()
        && !command_prompt.open
        && !command_prompt.has_blocking_overlay()
        && !sidebar.modal_active(command_prompt)
        && live_surface.owns_display(output_session, now_unix_ms())
}

fn looks_like_terminal_takeover_output(bytes: &[u8]) -> bool {
    let has_alt_screen = contains_escape_sequence(bytes, b"\x1b[?1049h")
        || contains_escape_sequence(bytes, b"\x1b[?1047h")
        || contains_escape_sequence(bytes, b"\x1b[?1048h");
    let has_application_cursor = contains_escape_sequence(bytes, b"\x1b[?1h");
    let has_private_sync = contains_escape_sequence(bytes, b"\x1b[?2026h");
    let has_hide_cursor = contains_escape_sequence(bytes, b"\x1b[?25l");
    let has_cursor_positioning = bytes.contains(&b'H') && contains_escape_sequence(bytes, b"\x1b[");
    let has_clear = contains_escape_sequence(bytes, b"\x1b[2J");
    let has_sync_takeover = has_private_sync
        && (has_alt_screen || has_application_cursor || has_hide_cursor || has_clear);

    has_alt_screen
        || has_application_cursor
        || has_sync_takeover
        || (has_hide_cursor && (has_cursor_positioning || has_clear))
}

fn build_ui_write_payload(buffer: &str, synchronized_updates: bool) -> String {
    if synchronized_updates {
        format!("{ANSI_SYNC_UPDATE_START}{buffer}{ANSI_SYNC_UPDATE_END}")
    } else {
        buffer.to_string()
    }
}

#[cfg(test)]
fn strip_sync_update_markers(bytes: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;

    while index < bytes.len() {
        if bytes[index..].starts_with(ANSI_SYNC_UPDATE_START.as_bytes()) {
            index += ANSI_SYNC_UPDATE_START.len();
            continue;
        }
        if bytes[index..].starts_with(ANSI_SYNC_UPDATE_END.as_bytes()) {
            index += ANSI_SYNC_UPDATE_END.len();
            continue;
        }
        output.push(bytes[index]);
        index += 1;
    }

    output
}

#[cfg(test)]
fn strip_sync_update_markers_stream(bytes: &[u8], pending_tail: &mut Vec<u8>) -> Vec<u8> {
    let mut combined = Vec::with_capacity(pending_tail.len() + bytes.len());
    combined.extend_from_slice(pending_tail);
    combined.extend_from_slice(bytes);
    pending_tail.clear();

    let tail_len = sync_marker_partial_suffix_len(&combined);
    let split_at = combined.len().saturating_sub(tail_len);
    if tail_len > 0 {
        pending_tail.extend_from_slice(&combined[split_at..]);
    }

    strip_sync_update_markers(&combined[..split_at])
}

fn extract_live_output_batches(
    bytes: &[u8],
    pending_tail: &mut Vec<u8>,
    pending_batch: &mut Vec<u8>,
    sync_batch_open: &mut bool,
) -> Vec<Vec<u8>> {
    let mut combined = Vec::with_capacity(pending_tail.len() + bytes.len());
    combined.extend_from_slice(pending_tail);
    combined.extend_from_slice(bytes);
    pending_tail.clear();

    let tail_len = sync_marker_partial_suffix_len(&combined);
    let split_at = combined.len().saturating_sub(tail_len);
    if tail_len > 0 {
        pending_tail.extend_from_slice(&combined[split_at..]);
    }

    let mut batches = Vec::new();
    let mut immediate = Vec::new();
    let mut index = 0;
    while index < split_at {
        if combined[index..split_at].starts_with(ANSI_SYNC_UPDATE_START.as_bytes()) {
            if !*sync_batch_open && !immediate.is_empty() {
                batches.push(std::mem::take(&mut immediate));
            }
            *sync_batch_open = true;
            index += ANSI_SYNC_UPDATE_START.len();
            continue;
        }
        if combined[index..split_at].starts_with(ANSI_SYNC_UPDATE_END.as_bytes()) {
            if *sync_batch_open {
                batches.push(std::mem::take(pending_batch));
                *sync_batch_open = false;
            }
            index += ANSI_SYNC_UPDATE_END.len();
            continue;
        }

        if *sync_batch_open {
            pending_batch.push(combined[index]);
        } else {
            immediate.push(combined[index]);
        }
        index += 1;
    }

    if !immediate.is_empty() {
        batches.push(immediate);
    }

    batches
}

fn sync_marker_partial_suffix_len(bytes: &[u8]) -> usize {
    let markers = [
        ANSI_SYNC_UPDATE_START.as_bytes(),
        ANSI_SYNC_UPDATE_END.as_bytes(),
    ];
    let max_len = markers
        .iter()
        .map(|marker| marker.len().saturating_sub(1))
        .max()
        .unwrap_or(0);

    for len in (1..=max_len.min(bytes.len())).rev() {
        let suffix = &bytes[bytes.len() - len..];
        if markers
            .iter()
            .any(|marker| marker.starts_with(suffix) && marker.len() > len)
        {
            return len;
        }
    }

    0
}

fn looks_like_terminal_probe_output(bytes: &[u8]) -> bool {
    contains_escape_sequence(bytes, b"\x1b[6n")
        || contains_escape_sequence(bytes, b"\x1b[c")
        || contains_escape_sequence(bytes, b"\x1b[>7u")
        || contains_escape_sequence(bytes, b"\x1b[?1004h")
        || contains_escape_sequence(bytes, b"\x1b]10;?")
}

fn looks_like_terminal_release_output(bytes: &[u8]) -> bool {
    contains_escape_sequence(bytes, b"\x1b[?1049l")
        || contains_escape_sequence(bytes, b"\x1b[?1047l")
        || contains_escape_sequence(bytes, b"\x1b[?1048l")
}

fn output_is_substantive(bytes: &[u8]) -> bool {
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            0x1b => {
                if index + 1 >= bytes.len() {
                    break;
                }
                match bytes[index + 1] {
                    b'[' => {
                        index += 2;
                        while index < bytes.len() {
                            let byte = bytes[index];
                            index += 1;
                            if (0x40..=0x7e).contains(&byte) {
                                break;
                            }
                        }
                    }
                    b']' => {
                        index += 2;
                        while index < bytes.len() {
                            match bytes[index] {
                                0x07 => {
                                    index += 1;
                                    break;
                                }
                                0x1b if index + 1 < bytes.len() && bytes[index + 1] == b'\\' => {
                                    index += 2;
                                    break;
                                }
                                _ => index += 1,
                            }
                        }
                    }
                    _ => index += 2,
                }
            }
            byte if byte < 0x20 || byte == 0x7f => {
                index += 1;
            }
            byte if byte < 0x80 => {
                if !(byte as char).is_ascii_whitespace() {
                    return true;
                }
                index += 1;
            }
            _ => return true,
        }
    }

    false
}

#[cfg(test)]
fn live_output_needs_chrome_redraw(bytes: &[u8]) -> bool {
    looks_like_terminal_release_output(bytes)
        || looks_like_terminal_destructive_repaint_output(bytes)
        || live_output_sidebar_damage(bytes).is_some()
}

fn live_output_can_skip_snapshot_redraw(
    live_surface: &LiveSurfaceState,
    output_session: &SessionAddress,
    bytes: &[u8],
    now_unix_ms: u128,
) -> bool {
    looks_like_terminal_takeover_output(bytes)
        || looks_like_terminal_probe_output(bytes)
        || (live_surface.pending_redraw
            && live_surface.is_known_live_command(output_session)
            && live_surface.is_bootstrapping(output_session, now_unix_ms))
}

#[cfg(test)]
fn live_output_sidebar_damage(bytes: &[u8]) -> Option<(usize, usize)> {
    let mut index = 0;
    let mut cursor_row = 1usize;
    let mut scroll_region: Option<(usize, usize)> = None;
    let mut damage: Option<(usize, usize)> = None;
    let mut cursor_known = false;

    while index + 2 < bytes.len() {
        if bytes[index] == 0x1b && index + 1 < bytes.len() && bytes[index + 1] == b'M' {
            let (start, end) = scroll_region.unwrap_or((1, usize::MAX));
            damage = Some(match damage {
                Some((previous_start, previous_end)) => {
                    (previous_start.min(start), previous_end.max(end))
                }
                None => (start, end),
            });
            index += 2;
            continue;
        }

        if bytes[index] != 0x1b || bytes[index + 1] != b'[' {
            index += 1;
            continue;
        }

        let sequence_start = index + 2;
        let mut sequence_end = sequence_start;
        while sequence_end < bytes.len() {
            let byte = bytes[sequence_end];
            if (0x40..=0x7e).contains(&byte) {
                break;
            }
            sequence_end += 1;
        }
        if sequence_end >= bytes.len() {
            break;
        }

        let final_byte = bytes[sequence_end];
        let params = std::str::from_utf8(&bytes[sequence_start..sequence_end]).ok();
        match final_byte {
            b'H' | b'f' => {
                let mut parts = params.unwrap_or("").split(';');
                let row = parts
                    .next()
                    .and_then(|value| {
                        if value.is_empty() {
                            Some(1)
                        } else {
                            value.parse::<usize>().ok()
                        }
                    })
                    .unwrap_or(1);
                cursor_row = row.max(1);
                cursor_known = true;
            }
            b'K' => {
                let row = if cursor_known { cursor_row.max(1) } else { 1 };
                damage = Some(match damage {
                    Some((previous_start, previous_end)) => {
                        (previous_start.min(row), previous_end.max(row))
                    }
                    None => (row, row),
                });
            }
            b'J' => {
                let mode = params
                    .unwrap_or("")
                    .split(';')
                    .next()
                    .and_then(|value| {
                        if value.is_empty() {
                            Some(0)
                        } else {
                            value.parse::<usize>().ok()
                        }
                    })
                    .unwrap_or(0);
                let affected = match mode {
                    0 => {
                        if cursor_known {
                            Some((cursor_row.max(1), usize::MAX))
                        } else {
                            Some((1, usize::MAX))
                        }
                    }
                    1 => {
                        if cursor_known {
                            Some((1, cursor_row.max(1)))
                        } else {
                            Some((1, usize::MAX))
                        }
                    }
                    2 | 3 => Some((1, usize::MAX)),
                    _ => None,
                };
                if let Some((start, end)) = affected {
                    damage = Some(match damage {
                        Some((previous_start, previous_end)) => {
                            (previous_start.min(start), previous_end.max(end))
                        }
                        None => (start, end),
                    });
                }
            }
            b'r' => {
                let mut parts = params.unwrap_or("").split(';');
                let top = parts
                    .next()
                    .and_then(|value| {
                        if value.is_empty() {
                            Some(1)
                        } else {
                            value.parse::<usize>().ok()
                        }
                    })
                    .unwrap_or(1)
                    .max(1);
                let bottom = parts
                    .next()
                    .and_then(|value| {
                        if value.is_empty() {
                            None
                        } else {
                            value.parse::<usize>().ok()
                        }
                    })
                    .unwrap_or(usize::MAX)
                    .max(top);
                scroll_region = Some((top, bottom));
            }
            _ => {}
        }

        index = sequence_end + 1;
    }

    damage
}

fn live_output_requires_full_sidebar_redraw(bytes: &[u8]) -> bool {
    looks_like_terminal_takeover_output(bytes)
        || looks_like_terminal_release_output(bytes)
        || contains_escape_sequence(bytes, b"\x1b[2J")
        || contains_escape_sequence(bytes, b"\x1b[1J")
        || contains_escape_sequence(bytes, b"\x1b[3J")
}

fn live_output_requires_full_sidebar_redraw_from_snapshots(
    before: Option<&crate::terminal::ScreenSnapshot>,
    after: Option<&crate::terminal::ScreenSnapshot>,
) -> bool {
    let (before, after) = match (before, after) {
        (Some(before), Some(after)) => (before, after),
        _ => return false,
    };

    before.size != after.size
        || before.alternate_screen != after.alternate_screen
        || before.scroll_top != after.scroll_top
        || before.scroll_bottom != after.scroll_bottom
}

#[cfg(test)]
fn looks_like_terminal_destructive_repaint_output(bytes: &[u8]) -> bool {
    contains_escape_sequence(bytes, b"\x1b[J")
        || contains_escape_sequence(bytes, b"\x1b[0J")
        || contains_escape_sequence(bytes, b"\x1b[1J")
        || contains_escape_sequence(bytes, b"\x1b[2J")
}

fn looks_like_shell_prompt_output(bytes: &[u8]) -> bool {
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut index = 0;

    while index < bytes.len() {
        match bytes[index] {
            0x1b => {
                if index + 1 >= bytes.len() {
                    break;
                }
                match bytes[index + 1] {
                    b'[' => {
                        index += 2;
                        while index < bytes.len() {
                            let byte = bytes[index];
                            index += 1;
                            if (0x40..=0x7e).contains(&byte) {
                                break;
                            }
                        }
                    }
                    b']' => {
                        index += 2;
                        while index < bytes.len() {
                            match bytes[index] {
                                0x07 => {
                                    index += 1;
                                    break;
                                }
                                0x1b if index + 1 < bytes.len() && bytes[index + 1] == b'\\' => {
                                    index += 2;
                                    break;
                                }
                                _ => index += 1,
                            }
                        }
                    }
                    _ => index += 2,
                }
            }
            b'\r' => {
                current.clear();
                index += 1;
            }
            b'\n' => {
                if !current.trim().is_empty() {
                    lines.push(current.clone());
                }
                current.clear();
                index += 1;
            }
            byte if byte < 0x20 || byte == 0x7f => {
                index += 1;
            }
            byte if byte < 0x80 => {
                current.push(byte as char);
                index += 1;
            }
            _ => return false,
        }
    }

    let candidate = if current.trim().is_empty() {
        lines
            .iter()
            .rev()
            .find(|line| !line.trim().is_empty())
            .map(|line| line.as_str())
            .unwrap_or_default()
    } else {
        current.as_str()
    };
    let trimmed = candidate.trim_end();
    if trimmed.is_empty() || trimmed.len() > 160 {
        return false;
    }

    matches!(trimmed.chars().last(), Some('$' | '#' | '%' | '>' | ':'))
}

fn contains_escape_sequence(bytes: &[u8], needle: &[u8]) -> bool {
    bytes.windows(needle.len()).any(|window| window == needle)
}

fn parse_console_action(
    bytes: &[u8],
    allow_escape_dismiss: bool,
    allow_interrupt_exit: bool,
) -> Option<ConsoleAction> {
    if shortcut_matches(bytes, SHORTCUT_INTERRUPT_EXIT) && allow_interrupt_exit {
        return Some(ConsoleAction::QuitHost);
    }
    if bytes == [0x1b] && allow_escape_dismiss {
        return Some(ConsoleAction::DismissOverlay);
    }
    if shortcut_matches(bytes, SHORTCUT_PREVIOUS_SESSION) || bytes == b"\x1bp" || bytes == b"\x1b[Z"
    {
        return Some(ConsoleAction::PreviousSession);
    }
    if shortcut_matches(bytes, SHORTCUT_NEXT_SESSION) || bytes == b"\x1bn" || bytes == b"\x1b[1;5I"
    {
        return Some(ConsoleAction::NextSession);
    }
    if shortcut_matches(bytes, SHORTCUT_NEW_SESSION) || bytes == b"\x1bc" {
        return Some(ConsoleAction::CreateSession);
    }
    if shortcut_matches(bytes, SHORTCUT_FULLSCREEN) {
        return Some(ConsoleAction::EnterNativeFullscreen);
    }
    if shortcut_matches(bytes, SHORTCUT_LIST_SESSIONS) {
        return Some(ConsoleAction::ListSessions);
    }
    if shortcut_matches(bytes, SHORTCUT_CLOSE_SESSION) {
        return Some(ConsoleAction::CloseCurrentSession);
    }
    if shortcut_matches(bytes, SHORTCUT_QUIT) || bytes == b"\x1bx" {
        return Some(ConsoleAction::QuitHost);
    }
    if bytes == b"\x1bv" {
        return Some(ConsoleAction::TogglePeek);
    }
    match bytes {
        [0x1b, digit @ b'1'..=b'9'] => Some(ConsoleAction::FocusIndex((digit - b'0') as usize)),
        _ => None,
    }
}

fn describe_shortcut_matches(bytes: &[u8]) -> String {
    let mut labels = Vec::new();
    if shortcut_matches(bytes, COMMAND_BAR_PREFIX)
        || shortcut_matches(bytes, COMMAND_BAR_PREFIX_FALLBACK)
    {
        labels.push("command_bar");
    }
    if shortcut_matches(bytes, SHORTCUT_FULLSCREEN) {
        labels.push("fullscreen");
    }
    if shortcut_matches(bytes, SHORTCUT_NEW_SESSION) {
        labels.push("new_session");
    }
    if shortcut_matches(bytes, SHORTCUT_NEXT_SESSION) {
        labels.push("next_session");
    }
    if shortcut_matches(bytes, SHORTCUT_PREVIOUS_SESSION) {
        labels.push("previous_session");
    }
    if shortcut_matches(bytes, SHORTCUT_LIST_SESSIONS) {
        labels.push("list_sessions");
    }
    if shortcut_matches(bytes, SHORTCUT_CLOSE_SESSION) {
        labels.push("close_session");
    }
    if shortcut_matches(bytes, SHORTCUT_QUIT) {
        labels.push("quit");
    }
    if shortcut_matches(bytes, SHORTCUT_INTERRUPT_EXIT) {
        labels.push("interrupt");
    }
    if labels.is_empty() {
        "none".to_string()
    } else {
        labels.join(",")
    }
}

fn escape_input_bytes(bytes: &[u8]) -> String {
    let mut escaped = String::new();
    for &byte in bytes {
        match byte {
            b'\n' => escaped.push_str("\\n"),
            b'\r' => escaped.push_str("\\r"),
            b'\t' => escaped.push_str("\\t"),
            0x1b => escaped.push_str("\\e"),
            0x20..=0x7e => escaped.push(byte as char),
            _ => escaped.push_str(&format!("\\x{byte:02x}")),
        }
    }
    escaped
}

fn shortcut_matches(bytes: &[u8], shortcut: u8) -> bool {
    bytes == [shortcut] || decode_encoded_control_shortcut(bytes) == Some(shortcut)
}

fn decode_encoded_control_shortcut(bytes: &[u8]) -> Option<u8> {
    decode_csi_u_control_shortcut(bytes)
        .or_else(|| decode_modify_other_keys_control_shortcut(bytes))
}

fn decode_csi_u_control_shortcut(bytes: &[u8]) -> Option<u8> {
    let body = bytes.strip_prefix(b"\x1b[")?.strip_suffix(b"u")?;
    let split = body.iter().position(|byte| *byte == b';')?;
    let codepoint = std::str::from_utf8(&body[..split])
        .ok()?
        .parse::<u32>()
        .ok()?;
    let modifier = std::str::from_utf8(&body[split + 1..])
        .ok()?
        .parse::<u32>()
        .ok()?;
    decode_control_shortcut_from_parts(codepoint, modifier)
}

fn decode_modify_other_keys_control_shortcut(bytes: &[u8]) -> Option<u8> {
    let body = bytes.strip_prefix(b"\x1b[27;")?.strip_suffix(b"~")?;
    let split = body.iter().position(|byte| *byte == b';')?;
    let modifier = std::str::from_utf8(&body[..split])
        .ok()?
        .parse::<u32>()
        .ok()?;
    let codepoint = std::str::from_utf8(&body[split + 1..])
        .ok()?
        .parse::<u32>()
        .ok()?;
    decode_control_shortcut_from_parts(codepoint, modifier)
}

fn decode_control_shortcut_from_parts(codepoint: u32, modifier: u32) -> Option<u8> {
    let ctrl_active = modifier > 0 && ((modifier - 1) & 0b100) != 0;
    if !ctrl_active {
        return None;
    }
    let ascii = u8::try_from(codepoint).ok()?;
    let letter = ascii.to_ascii_lowercase();
    letter.is_ascii_lowercase().then_some(letter & 0x1f)
}

fn consume_input_escape(bytes: &[u8], index: usize) -> Option<usize> {
    if index + 1 >= bytes.len() {
        return None;
    }

    match bytes[index + 1] {
        b'[' => {
            let mut cursor = index + 2;
            while cursor < bytes.len() {
                let byte = bytes[cursor];
                if (0x40..=0x7e).contains(&byte) {
                    return Some(cursor + 1);
                }
                cursor += 1;
            }
            None
        }
        b'O' => (index + 2 < bytes.len()).then_some(index + 3),
        _ => Some((index + 2).min(bytes.len())),
    }
}

fn is_typing_byte(byte: u8) -> bool {
    byte >= 0x20 && byte != 0x7f
}

fn sidebar_layout(width: usize, collapsed: bool) -> Option<(usize, usize)> {
    if width < 64 {
        return None;
    }

    let sidebar_width = if collapsed {
        COLLAPSED_SIDEBAR_WIDTH
    } else if width >= 120 {
        36
    } else if width >= 96 {
        32
    } else {
        28
    };
    let separator_col = width.checked_sub(sidebar_width)?;
    (separator_col > 20).then_some((separator_col, sidebar_width))
}

fn style_sidebar_divider() -> String {
    format!("{ANSI_FG_FOOTER_DIVIDER}┃{ANSI_RESET}")
}

fn style_sidebar_header_line(line: &str, width: usize) -> String {
    format!(
        "{ANSI_BG_SIDEBAR_HEADER}{}{ANSI_RESET}",
        pad_line(line, width)
    )
}

fn style_sidebar_hint_line(line: &str, width: usize) -> String {
    format!(
        "{ANSI_BG_SIDEBAR_HINT}{}{ANSI_RESET}",
        pad_line(line, width)
    )
}

fn style_sidebar_item_line(line: &str, width: usize, active: bool) -> String {
    let style = if active {
        ANSI_BG_SIDEBAR_ACTIVE
    } else {
        ANSI_BG_SIDEBAR_ITEM
    };
    format!("{style}{}{ANSI_RESET}", pad_line(line, width))
}

fn style_sidebar_detail_line(line: &str, width: usize) -> String {
    format!(
        "{ANSI_BG_SIDEBAR_DETAIL}{}{ANSI_RESET}",
        pad_line(line, width)
    )
}

fn build_collapsed_sidebar_lines(row_capacity: usize, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    if row_capacity == 0 {
        return lines;
    }

    lines.push(style_sidebar_hint_line("←", width));
    while lines.len() < row_capacity {
        lines.push(style_sidebar_item_line("", width, false));
    }
    lines
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SidebarDisplayStatus {
    Running,
    Input,
    Confirm,
}

fn sidebar_display_status(
    session: &crate::session::SessionRecord,
    waiting: bool,
) -> SidebarDisplayStatus {
    if waiting || matches!(session.status, SessionStatus::WaitingInput) {
        if session_requires_confirmation(session) {
            SidebarDisplayStatus::Confirm
        } else {
            SidebarDisplayStatus::Input
        }
    } else {
        SidebarDisplayStatus::Running
    }
}

fn sidebar_status_badge(status: SidebarDisplayStatus, now_unix_ms: u128) -> &'static str {
    match status {
        SidebarDisplayStatus::Running => match ((now_unix_ms / 500) % 3) as usize {
            0 => "🍳R",
            1 => "🍲R",
            _ => "🔥R",
        },
        SidebarDisplayStatus::Input => "🔊I",
        SidebarDisplayStatus::Confirm => "📢C",
    }
}

fn sidebar_item_style(active: bool) -> &'static str {
    if active {
        ANSI_BG_SIDEBAR_ACTIVE
    } else {
        ANSI_BG_SIDEBAR_ITEM
    }
}

fn style_sidebar_badge(
    status: SidebarDisplayStatus,
    now_unix_ms: u128,
    active: bool,
) -> (String, usize) {
    let badge = sidebar_status_badge(status, now_unix_ms);
    let color = match status {
        SidebarDisplayStatus::Running => ANSI_FG_SIDEBAR_RUNNING,
        SidebarDisplayStatus::Input => ANSI_FG_SIDEBAR_INPUT,
        SidebarDisplayStatus::Confirm => ANSI_FG_SIDEBAR_CONFIRM,
    };
    let base_style = sidebar_item_style(active);
    (
        format!("{color}{badge}{base_style}"),
        display_width(badge.chars()) as usize,
    )
}

fn sidebar_status_label(status: SidebarDisplayStatus) -> &'static str {
    match status {
        SidebarDisplayStatus::Running => "RUNNING",
        SidebarDisplayStatus::Input => "INPUT",
        SidebarDisplayStatus::Confirm => "CONFIRM",
    }
}

fn sidebar_base_label(session: &crate::session::SessionRecord) -> String {
    let title = session.title.trim();
    if !title.is_empty() {
        return title.to_string();
    }

    session
        .command_line
        .split_whitespace()
        .next()
        .and_then(|value| Path::new(value).file_name().and_then(|name| name.to_str()))
        .filter(|value| !value.is_empty())
        .unwrap_or("bash")
        .to_string()
}

fn sidebar_session_label(session: &crate::session::SessionRecord) -> String {
    format!(
        "{}@{}",
        sidebar_base_label(session),
        session.address().node_id()
    )
}

fn session_requires_confirmation(session: &crate::session::SessionRecord) -> bool {
    let Some(screen_state) = session.screen_state.as_ref() else {
        return false;
    };
    let snapshot = screen_state.active_snapshot();
    snapshot.lines.iter().any(|line| {
        let visible = line.trim().to_ascii_lowercase();
        !visible.is_empty()
            && (visible.contains("approve")
                || visible.contains("approval")
                || visible.contains("confirm")
                || visible.contains("continue?")
                || visible.contains("allow")
                || visible.contains("permission")
                || visible.contains("[y/n]")
                || visible.contains("(y/n)")
                || visible.contains("yes/no"))
    })
}

fn build_sidebar_detail_source(session: &crate::session::SessionRecord, waiting: bool) -> String {
    let status = sidebar_display_status(session, waiting);
    format!(
        "{} | {}",
        sidebar_session_label(session),
        sidebar_status_label(status),
    )
}

fn build_sidebar_detail_text(detail: &str, width: usize) -> String {
    let text = take_display_width(detail.chars(), width);
    let padding = width.saturating_sub(display_width(text.chars()) as usize);
    format!("{}{}", " ".repeat(padding), text)
}

fn format_sidebar_item(
    session: &crate::session::SessionRecord,
    selected: bool,
    waiting: bool,
    now_unix_ms: u128,
    width: usize,
) -> String {
    let status = sidebar_display_status(session, waiting);
    let (badge, badge_width) = style_sidebar_badge(status, now_unix_ms, selected);
    let label = sidebar_session_label(session);
    let available_label_width = width.saturating_sub(2 + badge_width);
    let mut label = take_display_width(label.chars(), available_label_width);
    let padding = available_label_width.saturating_sub(display_width(label.chars()) as usize);
    label.push_str(&" ".repeat(padding));
    let marker = if selected { ">" } else { " " };
    let style = sidebar_item_style(selected);
    format!("{style}{marker} {label} {badge}{ANSI_RESET}")
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

fn style_footer_separator_line(width: usize) -> String {
    let line = "━".repeat(width);
    format!("{ANSI_FG_FOOTER_DIVIDER}{line}{ANSI_RESET}")
}

fn style_footer_separator_line_for_sidebar(
    width: usize,
    sidebar: Option<&SidebarOverlay>,
) -> String {
    let Some(sidebar) = sidebar else {
        return style_footer_separator_line(width);
    };

    let main_width = sidebar.separator_col.saturating_sub(1);
    let divider = "┃";
    let tail_width = width.saturating_sub(main_width + divider.chars().count());
    format!(
        "{ANSI_FG_FOOTER_DIVIDER}{}{ANSI_RESET}{}{ANSI_BG_SIDEBAR_ITEM}{}{ANSI_RESET}",
        "━".repeat(main_width),
        divider,
        " ".repeat(tail_width)
    )
}

#[cfg(test)]
fn build_full_frame_buffer(
    frame_text: &str,
    cursor: CursorPlacement,
    cursor_visible: bool,
    terminal_rows: u16,
) -> String {
    build_full_frame_buffer_with_sidebar(frame_text, None, cursor, cursor_visible, terminal_rows)
}

#[cfg(test)]
fn build_full_frame_buffer_with_sidebar(
    frame_text: &str,
    sidebar: Option<&SidebarOverlay>,
    cursor: CursorPlacement,
    cursor_visible: bool,
    terminal_rows: u16,
) -> String {
    build_full_frame_buffer_with_sidebar_diff(
        frame_text,
        None,
        sidebar,
        None,
        cursor,
        cursor_visible,
        terminal_rows,
    )
}

fn build_full_frame_buffer_with_sidebar_diff(
    frame_text: &str,
    previous_frame_lines: Option<&[String]>,
    sidebar: Option<&SidebarOverlay>,
    previous_sidebar: Option<&SidebarOverlay>,
    cursor: CursorPlacement,
    cursor_visible: bool,
    terminal_rows: u16,
) -> String {
    let mut buffer = String::from(RESET_FRAME_CURSOR);
    buffer.push_str("\x1b[?25l");
    let lines = if frame_text.is_empty() {
        Vec::new()
    } else {
        frame_text.split("\r\n").collect::<Vec<_>>()
    };
    let total_rows = sidebar
        .map(|overlay| overlay.lines.len().max(lines.len()))
        .unwrap_or(lines.len());

    for row in 1..=total_rows {
        let line = lines.get(row - 1).copied().unwrap_or("");
        let previous_main_line = previous_frame_lines
            .and_then(|previous| previous.get(row - 1))
            .map(String::as_str)
            .unwrap_or("");
        let redraw_main_row = previous_frame_lines.is_none() || previous_main_line != line;
        if let Some(overlay) = sidebar {
            if let Some(sidebar_line) = overlay.lines.get(row - 1) {
                let separator_col = overlay.separator_col;
                let content_col = overlay.content_col;
                let main_width = separator_col.saturating_sub(1);
                let (main_line, main_line_width) = truncate_ansi_line_with_width(line, main_width);
                if redraw_main_row {
                    let padding = " ".repeat(main_width.saturating_sub(main_line_width));
                    buffer.push_str(&format!(
                        "\x1b[{row};1H{main_line}{padding}",
                    ));
                }
                let previous_sidebar_line =
                    previous_sidebar.and_then(|previous| previous.lines.get(row - 1));
                let redraw_sidebar_row = previous_sidebar_line != Some(sidebar_line)
                    || previous_sidebar
                        .map(|previous| {
                            previous.separator_col != separator_col
                                || previous.content_col != content_col
                        })
                        .unwrap_or(true);
                if redraw_sidebar_row {
                    let fill_style = leading_ansi_style_prefix(sidebar_line);
                    buffer.push_str(&format!(
                        "\x1b[{row};{separator_col}H{}\x1b[{row};{content_col}H{fill_style}\x1b[K\x1b[{row};{content_col}H{sidebar_line}",
                        overlay.divider,
                    ));
                }
                continue;
            }
        }
        if redraw_main_row {
            buffer.push_str(&format!("\x1b[{row};1H\x1b[K\x1b[{row};1H{line}"));
        }
    }

    let previous_total_rows = previous_frame_lines
        .filter(|previous| !previous.is_empty())
        .map(|previous| previous.len())
        .unwrap_or(terminal_rows.max(1) as usize)
        .max(
            previous_sidebar
                .map(|previous| previous.lines.len())
                .unwrap_or(0),
        );
    let clear_start_row = if total_rows == 0 { 1 } else { total_rows + 1 };
    if clear_start_row <= previous_total_rows {
        for row in clear_start_row..=previous_total_rows {
            buffer.push_str(&format!("\x1b[{row};1H\x1b[K"));
        }
    }

    buffer.push_str(&format!(
        "\x1b[{};{}H{}",
        cursor.row.saturating_add(1),
        cursor.col.saturating_add(1),
        if cursor_visible {
            "\x1b[?25h"
        } else {
            "\x1b[?25l"
        }
    ));
    buffer
}

fn split_frame_lines(frame_text: &str) -> Vec<String> {
    if frame_text.is_empty() {
        Vec::new()
    } else {
        frame_text.split("\r\n").map(ToOwned::to_owned).collect()
    }
}

fn build_sidebar_overlay_buffer(
    sidebar: &SidebarOverlay,
    previous_sidebar: Option<&SidebarOverlay>,
    cursor: CursorPlacement,
    active_style_ansi: &str,
    cursor_visible: bool,
) -> String {
    let mut buffer = String::from("\x1b[?25l");
    let redraw_all = previous_sidebar
        .map(|previous| {
            previous.separator_col != sidebar.separator_col
                || previous.content_col != sidebar.content_col
                || previous.lines.len() != sidebar.lines.len()
        })
        .unwrap_or(true);

    for (index, sidebar_line) in sidebar.lines.iter().enumerate() {
        if !redraw_all
            && previous_sidebar.and_then(|previous| previous.lines.get(index)) == Some(sidebar_line)
        {
            continue;
        }

        let row = index + 1;
        let fill_style = leading_ansi_style_prefix(sidebar_line);
        buffer.push_str(&format!(
            "\x1b[{row};{}H{}\x1b[{row};{}H{fill_style}\x1b[K\x1b[{row};{}H{}",
            sidebar.separator_col,
            sidebar.divider,
            sidebar.content_col,
            sidebar.content_col,
            sidebar_line
        ));
    }

    if let Some(previous) = previous_sidebar {
        if previous.lines.len() > sidebar.lines.len() {
            for row in sidebar.lines.len() + 1..=previous.lines.len() {
                let fill_style = previous
                    .lines
                    .get(row - 1)
                    .map(|line| leading_ansi_style_prefix(line))
                    .unwrap_or(ANSI_BG_SIDEBAR_ITEM);
                buffer.push_str(&format!(
                    "\x1b[{row};{}H{}\x1b[{row};{}H{fill_style}\x1b[K",
                    previous.separator_col, previous.divider, previous.content_col
                ));
            }
        }
    }

    buffer.push_str(&cursor_restore_sequence_with_visibility(
        cursor,
        active_style_ansi,
        cursor_visible,
    ));

    buffer
}

fn live_surface_overlay_cursor_restore(snapshot: &crate::terminal::ScreenSnapshot) -> String {
    cursor_restore_sequence(
        CursorPlacement {
            row: snapshot.cursor_row,
            col: snapshot.cursor_col,
        },
        &snapshot.active_style_ansi,
    )
}

fn cursor_restore_sequence(cursor: CursorPlacement, active_style_ansi: &str) -> String {
    format!(
        "\x1b[{};{}H{}",
        cursor.row.saturating_add(1),
        cursor.col.saturating_add(1),
        active_style_ansi
    )
}

fn cursor_restore_sequence_with_visibility(
    cursor: CursorPlacement,
    active_style_ansi: &str,
    cursor_visible: bool,
) -> String {
    format!(
        "{}{}",
        cursor_restore_sequence(cursor, active_style_ansi),
        if cursor_visible {
            "\x1b[?25h"
        } else {
            "\x1b[?25l"
        }
    )
}

fn style_status_line(line: &str, width: usize) -> String {
    format!("{ANSI_BG_BAR}{}{ANSI_RESET}", pad_line(line, width))
}

fn leading_ansi_style_prefix(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut index = 0;

    while index < bytes.len() && bytes[index] == 0x1b {
        index += 1;
        if index >= bytes.len() {
            break;
        }
        match bytes[index] {
            b'[' => {
                index += 1;
                while index < bytes.len() {
                    let byte = bytes[index];
                    index += 1;
                    if (0x40..=0x7e).contains(&byte) {
                        break;
                    }
                }
            }
            b']' => {
                index += 1;
                while index < bytes.len() {
                    match bytes[index] {
                        0x07 => {
                            index += 1;
                            break;
                        }
                        0x1b if index + 1 < bytes.len() && bytes[index + 1] == b'\\' => {
                            index += 2;
                            break;
                        }
                        _ => index += 1,
                    }
                }
            }
            _ => {
                index += 1;
            }
        }
    }

    &line[..index.min(line.len())]
}

fn pad_line(line: &str, width: usize) -> String {
    let truncated = take_display_width(line.chars(), width);
    let padding = width.saturating_sub(display_width(truncated.chars()) as usize);
    format!("{truncated}{}", " ".repeat(padding))
}

fn truncate_ansi_line_with_width(line: &str, width: usize) -> (String, usize) {
    if width == 0 || line.is_empty() {
        return (ANSI_RESET.to_string(), 0);
    }

    let bytes = line.as_bytes();
    let mut index = 0;
    let mut visible = 0;
    let mut output = String::new();

    while index < bytes.len() {
        if bytes[index] == 0x1b {
            let escape_start = index;
            index += 1;
            if index >= bytes.len() {
                break;
            }
            match bytes[index] {
                b'[' => {
                    index += 1;
                    while index < bytes.len() {
                        let byte = bytes[index];
                        index += 1;
                        if (0x40..=0x7e).contains(&byte) {
                            break;
                        }
                    }
                }
                b']' => {
                    index += 1;
                    while index < bytes.len() {
                        match bytes[index] {
                            0x07 => {
                                index += 1;
                                break;
                            }
                            0x1b if index + 1 < bytes.len() && bytes[index + 1] == b'\\' => {
                                index += 2;
                                break;
                            }
                            _ => index += 1,
                        }
                    }
                }
                _ => {
                    index += 1;
                }
            }
            output.push_str(&line[escape_start..index.min(bytes.len())]);
            continue;
        }

        let Some(character) = line[index..].chars().next() else {
            break;
        };
        let next = char_display_width(character) as usize;
        if visible + next > width {
            break;
        }
        output.push(character);
        visible += next;
        index += character.len_utf8();
    }

    output.push_str(ANSI_RESET);
    (output, visible)
}

fn take_display_width(chars: impl IntoIterator<Item = char>, width: usize) -> String {
    let mut rendered = String::new();
    let mut used = 0;

    for ch in chars {
        let next = char_display_width(ch) as usize;
        if used + next > width {
            break;
        }
        rendered.push(ch);
        used += next;
    }

    rendered
}

fn display_width(chars: impl IntoIterator<Item = char>) -> u16 {
    chars.into_iter().map(char_display_width).sum()
}

fn char_display_width(ch: char) -> u16 {
    if ch.is_control() {
        0
    } else if matches!(
        ch as u32,
        0x1100..=0x115F
            | 0x2329..=0x232A
            | 0x2E80..=0xA4CF
            | 0xAC00..=0xD7A3
            | 0xF900..=0xFAFF
            | 0xFE10..=0xFE19
            | 0xFE30..=0xFE6F
            | 0xFF00..=0xFF60
            | 0xFFE0..=0xFFE6
            | 0x1F300..=0x1FAFF
    ) {
        2
    } else {
        1
    }
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

fn shell_title_from_command_line(command_line: &str) -> String {
    command_line
        .split_whitespace()
        .next()
        .map(shell_title)
        .unwrap_or_else(|| "bash".to_string())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ForegroundTitleDecision {
    Keep,
    SetLive(String),
    RestoreShell(String),
}

fn foreground_title_decision(
    shell_command_line: &str,
    foreground_name: Option<&str>,
    bootstrapping: bool,
) -> ForegroundTitleDecision {
    let shell_title = shell_title_from_command_line(shell_command_line);
    if let Some(name) = foreground_name {
        if let Some(live_title) = live_command_label(name) {
            return ForegroundTitleDecision::SetLive(live_title);
        }
        if !bootstrapping && name == shell_title {
            return ForegroundTitleDecision::RestoreShell(shell_title);
        }
    }

    ForegroundTitleDecision::Keep
}

fn nonfatal_foreground_process_inspection_error(error: &crate::pty::PtyError) -> bool {
    matches!(error, crate::pty::PtyError::Inspect(_))
}

fn managed_console_size(size: crate::terminal::TerminalSize) -> crate::terminal::TerminalSize {
    crate::terminal::TerminalSize {
        rows: size
            .rows
            .saturating_sub(MANAGED_CONSOLE_RESERVED_ROWS)
            .max(1),
        ..size
    }
}

fn workspace_viewport_size(
    size: crate::terminal::TerminalSize,
    sidebar_hidden: bool,
) -> crate::terminal::TerminalSize {
    let mut managed = managed_console_size(size);
    if let Some((separator_col, _sidebar_width)) =
        sidebar_layout(size.cols as usize, sidebar_hidden)
    {
        managed.cols = separator_col.saturating_sub(2) as u16;
    }
    managed
}

fn live_surface_target_size(
    _focused_live_session: bool,
    _keep_fullscreen: bool,
    terminal_size: crate::terminal::TerminalSize,
    sidebar_hidden: bool,
) -> crate::terminal::TerminalSize {
    workspace_viewport_size(terminal_size, sidebar_hidden)
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
    Lifecycle(crate::lifecycle::LifecycleError),
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
            Self::Lifecycle(error) => write!(f, "{error}"),
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

impl From<crate::lifecycle::LifecycleError> for AppError {
    fn from(value: crate::lifecycle::LifecycleError) -> Self {
        Self::Lifecycle(value)
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
        background_wait_notice, buffer_pending_runtime_events, build_full_frame_buffer,
        build_full_frame_buffer_with_sidebar, build_full_frame_buffer_with_sidebar_diff,
        build_sidebar_detail_source, build_sidebar_detail_text, build_ui_write_payload,
        bytes_include_submit, default_shell_program, extract_live_output_batches,
        extract_shell_prompt_command, foreground_title_decision, format_sidebar_item,
        live_command_label, live_command_label_from_shell_snapshot,
        live_output_can_skip_snapshot_redraw, live_output_needs_chrome_redraw,
        live_output_requires_full_sidebar_redraw,
        live_output_requires_full_sidebar_redraw_from_snapshots, live_output_sidebar_damage,
        live_overlay_visible, live_surface_target_size, looks_like_shell_prompt_output,
        looks_like_terminal_release_output, looks_like_terminal_takeover_output,
        native_fullscreen_seed_snapshot, next_runtime_event,
        nonfatal_foreground_process_inspection_error, now_unix_ms, output_is_substantive,
        parse_console_action, shell_title, shell_title_from_command_line, sidebar_display_status,
        strip_sync_update_markers, strip_sync_update_markers_stream, style_footer_separator_line,
        style_sidebar_badge, style_sidebar_divider, style_sidebar_header_line,
        style_sidebar_item_line, App, CommandOverlay, CommandPromptOutcome, CommandPromptState,
        ConsoleAction, CursorPlacement, ForegroundTitleDecision, ForwardInputNormalizer,
        InputTracker, LiveSurfaceState, NativeFullscreenState, PickerNavigationOutcome,
        RuntimeEvent, ShellCommandTracker, SidebarDisplayStatus, SidebarNavigationOutcome,
        SidebarOverlay, SidebarState, ANSI_BG_SIDEBAR_ACTIVE, ANSI_FG_SIDEBAR_CONFIRM,
        ANSI_FG_SIDEBAR_INPUT, ANSI_FG_SIDEBAR_RUNNING, ANSI_SYNC_UPDATE_END,
        ANSI_SYNC_UPDATE_START, LIVE_SURFACE_STATUS_ROWS, PICKER_ESCAPE_TIMEOUT_MS,
        SIDEBAR_NAVIGATION_TIMEOUT_MS,
        SHORTCUT_FULLSCREEN, SHORTCUT_INTERRUPT_EXIT, SHORTCUT_NEXT_SESSION,
        SHORTCUT_PREVIOUS_SESSION,
    };
    use crate::client::normalize_endpoint;
    use crate::config::AppConfig;
    use crate::console::ConsoleState;
    use crate::pty::PtyError;
    use crate::renderer::{Renderer, RendererState};
    use crate::scheduler::{SchedulerPhase, SchedulerState};
    use crate::session::{SessionAddress, SessionRegistry};
    use crate::terminal::{TerminalEngine, TerminalSize};
    use crate::transcript::TerminalTranscript;
    use std::collections::{HashMap, VecDeque};
    use std::io;
    use std::sync::mpsc;

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
                output_bytes_after_enter: 0,
                submitted_input_bytes: 3,
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
            parse_console_action(b"\x1bc", false, true),
            Some(ConsoleAction::CreateSession)
        );
        assert_eq!(
            parse_console_action(&[0x1b], true, true),
            Some(ConsoleAction::DismissOverlay)
        );
        assert_eq!(parse_console_action(&[0x1b], false, true), None);
        assert_eq!(
            parse_console_action(&[SHORTCUT_INTERRUPT_EXIT], false, false),
            None
        );
        assert_eq!(
            parse_console_action(&[SHORTCUT_INTERRUPT_EXIT], false, true),
            Some(ConsoleAction::QuitHost)
        );
        assert_eq!(
            parse_console_action(&[SHORTCUT_NEXT_SESSION], false, true),
            Some(ConsoleAction::NextSession)
        );
        assert_eq!(
            parse_console_action(&[SHORTCUT_FULLSCREEN], false, true),
            Some(ConsoleAction::EnterNativeFullscreen)
        );
        assert_eq!(
            parse_console_action(b"\x1b[111;5u", false, true),
            Some(ConsoleAction::EnterNativeFullscreen)
        );
        assert_eq!(
            parse_console_action(b"\x1b[27;5;111~", false, true),
            Some(ConsoleAction::EnterNativeFullscreen)
        );
        assert_eq!(
            parse_console_action(&[SHORTCUT_PREVIOUS_SESSION], false, true),
            Some(ConsoleAction::PreviousSession)
        );
        assert_eq!(
            parse_console_action(b"\x1bn", false, true),
            Some(ConsoleAction::NextSession)
        );
        assert_eq!(
            parse_console_action(b"\x1bp", false, true),
            Some(ConsoleAction::PreviousSession)
        );
        assert_eq!(
            parse_console_action(b"\x1b3", false, true),
            Some(ConsoleAction::FocusIndex(3))
        );
        assert_eq!(
            parse_console_action(b"\x1bv", false, true),
            Some(ConsoleAction::TogglePeek)
        );
        assert_eq!(
            parse_console_action(b"\x1bx", false, true),
            Some(ConsoleAction::QuitHost)
        );
        assert_eq!(parse_console_action(b"plain input", false, true), None);
    }

    #[test]
    fn command_prompt_opens_for_encoded_ctrl_w_shortcuts() {
        let mut prompt = CommandPromptState::default();

        assert_eq!(
            prompt.handle_input(b"\x1b[119;5u"),
            Some(CommandPromptOutcome::RenderOnly)
        );
        assert!(prompt.open);

        let mut prompt = CommandPromptState::default();
        assert_eq!(
            prompt.handle_input(b"\x1b[27;5;119~"),
            Some(CommandPromptOutcome::RenderOnly)
        );
        assert!(prompt.open);
    }

    #[test]
    fn native_fullscreen_allows_ctrl_o_toggle_exit() {
        let mut fullscreen = NativeFullscreenState::default();
        fullscreen.activate(SessionAddress::new("local", "session-1"));

        let outcome = fullscreen.handle_input(&[SHORTCUT_FULLSCREEN]);
        assert!(outcome.exit_requested);
        assert!(outcome.forwarded.is_empty());
    }

    #[test]
    fn native_fullscreen_forwards_non_toggle_input() {
        let mut fullscreen = NativeFullscreenState::default();
        fullscreen.activate(SessionAddress::new("local", "session-1"));

        let outcome = fullscreen.handle_input(b"a");
        assert!(!outcome.exit_requested);
        assert_eq!(outcome.forwarded, vec![b'a']);
    }

    #[test]
    fn native_fullscreen_allows_encoded_ctrl_o_toggle_exit() {
        let mut fullscreen = NativeFullscreenState::default();
        fullscreen.activate(SessionAddress::new("local", "session-1"));

        let csi_u = fullscreen.handle_input(b"\x1b[111;5u");
        assert!(csi_u.exit_requested);
        assert!(csi_u.forwarded.is_empty());

        let mut fullscreen = NativeFullscreenState::default();
        fullscreen.activate(SessionAddress::new("local", "session-1"));

        let modify_other_keys = fullscreen.handle_input(b"\x1b[27;5;111~");
        assert!(modify_other_keys.exit_requested);
        assert!(modify_other_keys.forwarded.is_empty());
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
    fn picker_accepts_application_cursor_navigation_sequences() {
        let mut registry = SessionRegistry::new();
        let first = registry.create_local_session(
            "local".to_string(),
            "bash".to_string(),
            "bash".to_string(),
        );
        let _second = registry.create_local_session(
            "local".to_string(),
            "codex".to_string(),
            "codex".to_string(),
        );
        let sessions = registry.list();

        let mut prompt = CommandPromptState::default();
        prompt.toggle_sessions(&sessions, Some(first.address()));

        assert_eq!(
            prompt.handle_picker_navigation(b"\x1bOB", &sessions, Some(first.address()), 100),
            Some(PickerNavigationOutcome::Render)
        );
        assert_eq!(
            prompt.selected_picker_index(&sessions, Some(first.address())),
            Some(2)
        );

        assert_eq!(
            prompt.handle_picker_navigation(b"\x1bOA", &sessions, Some(first.address()), 110),
            Some(PickerNavigationOutcome::Render)
        );
        assert_eq!(
            prompt.selected_picker_index(&sessions, Some(first.address())),
            Some(1)
        );
        assert_eq!(prompt.overlay, CommandOverlay::Sessions);
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

    #[test]
    fn session_overlay_submit_accepts_application_and_kitty_enter_sequences() {
        let mut registry = SessionRegistry::new();
        let first = registry.create_local_session(
            "local".to_string(),
            "bash".to_string(),
            "bash".to_string(),
        );
        let _second = registry.create_local_session(
            "local".to_string(),
            "codex".to_string(),
            "codex".to_string(),
        );
        let sessions = registry.list();

        let mut prompt = CommandPromptState::default();
        prompt.toggle_sessions(&sessions, Some(first.address()));
        prompt.handle_picker_navigation(b"\x1b[B", &sessions, Some(first.address()), 100);

        assert_eq!(
            prompt.handle_picker_navigation(&[0x1b], &sessions, Some(first.address()), 110),
            Some(PickerNavigationOutcome::Consumed)
        );
        assert_eq!(
            prompt.handle_picker_navigation(b"O", &sessions, Some(first.address()), 120),
            Some(PickerNavigationOutcome::Consumed)
        );
        assert_eq!(
            prompt.handle_picker_navigation(b"M", &sessions, Some(first.address()), 130),
            Some(PickerNavigationOutcome::Submit)
        );

        let mut kitty_prompt = CommandPromptState::default();
        kitty_prompt.toggle_sessions(&sessions, Some(first.address()));
        kitty_prompt.handle_picker_navigation(b"\x1b[B", &sessions, Some(first.address()), 200);
        assert_eq!(
            kitty_prompt.handle_picker_navigation(
                b"\x1b[13u",
                &sessions,
                Some(first.address()),
                210,
            ),
            Some(PickerNavigationOutcome::Submit)
        );

        assert!(prompt.submit_overlay(b"\x1bOM"));
        assert!(kitty_prompt.submit_overlay(b"\x1b[13u"));
    }

    #[test]
    fn notice_overlay_does_not_block_enter_and_clears_on_forwarded_input() {
        let mut prompt = CommandPromptState::default();
        prompt.set_message("Created new session.");

        assert!(!prompt.submit_overlay(b"\r"));
        assert!(prompt.clear_message_on_forwarded_input(b"\t"));
        assert_eq!(prompt.overlay, CommandOverlay::None);
    }

    #[test]
    fn passive_message_does_not_claim_escape() {
        let mut prompt = CommandPromptState::default();
        prompt.set_message("session-2 is waiting");

        assert!(!prompt.wants_escape_dismiss());
        assert_eq!(
            parse_console_action(&[0x1b], prompt.wants_escape_dismiss(), true),
            None
        );
    }

    #[test]
    fn passive_message_does_not_replace_blocking_overlay() {
        let mut prompt = CommandPromptState::default();
        prompt.overlay = CommandOverlay::Sessions;

        prompt.set_passive_message("session-2 is waiting");

        assert_eq!(prompt.overlay, CommandOverlay::Sessions);
    }

    #[test]
    fn passive_message_does_not_trigger_live_overlay_visibility() {
        let mut prompt = CommandPromptState::default();
        let sidebar = SidebarState::default();
        prompt.set_passive_message("session-2 is waiting");

        assert!(!live_overlay_visible(&prompt, &sidebar));

        prompt.toggle_help();
        assert!(live_overlay_visible(&prompt, &sidebar));
    }

    #[test]
    fn message_overlay_uses_status_line_without_adding_footer_rows() {
        let mut prompt = CommandPromptState::default();
        prompt.set_message("Created new session.");

        assert_eq!(prompt.overlay_lines(Vec::new(), None).len(), 1);
        assert_eq!(
            prompt.status_line("focus: none | mode: workspace-idle"),
            "notice: Created new session."
        );
    }

    #[test]
    fn session_picker_renders_styled_header_and_single_line_entries() {
        let mut registry = SessionRegistry::new();
        let first = registry.create_local_session(
            "local".to_string(),
            "bash".to_string(),
            "bash".to_string(),
        );
        let second = registry.create_local_session(
            "local".to_string(),
            "codex".to_string(),
            "codex".to_string(),
        );
        let mut engine = TerminalEngine::new(TerminalSize {
            rows: 2,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        });
        engine.feed(b"\x1b]0;k@k: /opt/data/workspace/test\x07");
        registry.update_screen_state(second.address(), engine.state());

        let sessions = registry.list();
        let mut prompt = CommandPromptState::default();
        prompt.toggle_sessions(&sessions, Some(first.address()));

        let lines = prompt.overlay_lines(sessions, Some(first.address()));

        assert_eq!(
            lines[0],
            "sessions: Up/Down move  ^B prev  ^F next  Enter select  Esc close  1-9 direct"
        );
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[1], ">  1. local/session-1 | bash | cwd: unknown");
        assert_eq!(
            lines[2],
            "   2. local/session-2 | codex | cwd: /opt/data/workspace/test"
        );
        assert!(lines[3].starts_with("keys:"));
    }

    #[test]
    fn sidebar_opens_moves_and_submits_selected_session() {
        let mut registry = SessionRegistry::new();
        let first = registry.create_local_session(
            "local".to_string(),
            "bash".to_string(),
            "bash".to_string(),
        );
        let second = registry.create_local_session(
            "local".to_string(),
            "codex".to_string(),
            "codex".to_string(),
        );
        let sessions = registry.list();
        let focused = Some(first.address());
        let prompt = CommandPromptState::default();
        let mut sidebar = SidebarState::default();

        assert_eq!(
            sidebar.handle_navigation(b"\x1b[C", &sessions, focused, true, &prompt, 100),
            Some(SidebarNavigationOutcome::Render)
        );
        assert_eq!(
            sidebar.handle_navigation(b"\x1b[B", &sessions, focused, true, &prompt, 110),
            Some(SidebarNavigationOutcome::Render)
        );
        assert_eq!(
            sidebar.handle_navigation(b"\r", &sessions, focused, true, &prompt, 120),
            Some(SidebarNavigationOutcome::Submit(second.address().clone()))
        );
        assert!(!sidebar.focused);
    }

    #[test]
    fn sidebar_hides_and_reopens_without_losing_selection() {
        let mut registry = SessionRegistry::new();
        let first = registry.create_local_session(
            "local".to_string(),
            "bash".to_string(),
            "bash".to_string(),
        );
        let second = registry.create_local_session(
            "local".to_string(),
            "codex".to_string(),
            "codex".to_string(),
        );
        let sessions = registry.list();
        let focused = Some(first.address());
        let prompt = CommandPromptState::default();
        let mut sidebar = SidebarState::default();

        assert!(!sidebar.hidden);
        assert_eq!(
            sidebar.handle_navigation(b"\x1b[C", &sessions, focused, true, &prompt, 100),
            Some(SidebarNavigationOutcome::Render)
        );
        assert_eq!(
            sidebar.handle_navigation(b"\x1b[B", &sessions, focused, true, &prompt, 110),
            Some(SidebarNavigationOutcome::Render)
        );
        assert_eq!(
            sidebar.selected_session(&sessions, focused),
            Some(second.address().clone())
        );

        assert_eq!(
            sidebar.handle_navigation(b"h", &sessions, focused, true, &prompt, 120),
            Some(SidebarNavigationOutcome::Render)
        );
        assert!(sidebar.hidden);
        assert!(!sidebar.focused);

        assert_eq!(
            sidebar.handle_navigation(b"\x1b[D", &sessions, focused, true, &prompt, 130),
            Some(SidebarNavigationOutcome::Render)
        );
        assert!(!sidebar.hidden);
        assert!(sidebar.focused);
        assert_eq!(
            sidebar.selected_session(&sessions, focused),
            Some(second.address().clone())
        );
    }

    #[test]
    fn sidebar_consumes_split_left_arrow_tail_after_escape_timeout() {
        let mut registry = SessionRegistry::new();
        let first = registry.create_local_session(
            "local".to_string(),
            "bash".to_string(),
            "bash".to_string(),
        );
        let sessions = registry.list();
        let focused = Some(first.address());
        let prompt = CommandPromptState::default();
        let mut sidebar = SidebarState::default();

        assert_eq!(
            sidebar.handle_navigation(b"\x1b[C", &sessions, focused, true, &prompt, 100),
            Some(SidebarNavigationOutcome::Render)
        );
        assert!(sidebar.focused);
        assert_eq!(
            sidebar.handle_navigation(b"\x1b", &sessions, focused, true, &prompt, 110),
            Some(SidebarNavigationOutcome::Consumed)
        );
        assert!(sidebar.flush_navigation_timeout(
            &prompt,
            110 + SIDEBAR_NAVIGATION_TIMEOUT_MS + 1
        ));
        assert!(!sidebar.focused);
        assert_eq!(
            sidebar.handle_navigation(
                b"[",
                &sessions,
                focused,
                true,
                &prompt,
                110 + SIDEBAR_NAVIGATION_TIMEOUT_MS + 10,
            ),
            Some(SidebarNavigationOutcome::Consumed)
        );
        assert_eq!(
            sidebar.handle_navigation(
                b"D",
                &sessions,
                focused,
                true,
                &prompt,
                110 + SIDEBAR_NAVIGATION_TIMEOUT_MS + 20,
            ),
            Some(SidebarNavigationOutcome::Consumed)
        );
        assert!(!sidebar.focused);
        assert!(sidebar.pending_navigation_escape.is_empty());
    }

    #[test]
    fn sidebar_leaves_unfocused_left_arrow_available_for_session_passthrough() {
        let mut registry = SessionRegistry::new();
        let first = registry.create_local_session(
            "local".to_string(),
            "bash".to_string(),
            "bash".to_string(),
        );
        let sessions = registry.list();
        let focused = Some(first.address());
        let prompt = CommandPromptState::default();
        let sidebar = SidebarState::default();

        assert_eq!(
            sidebar.clone().handle_navigation(b"\x1b[D", &sessions, focused, true, &prompt, 100),
            None
        );
    }

    #[test]
    fn sidebar_item_renders_title_with_node_and_running_badge() {
        let mut registry = SessionRegistry::new();
        let session = registry.create_local_session(
            "local".to_string(),
            "bash".to_string(),
            "/bin/bash".to_string(),
        );

        let line = format_sidebar_item(&session, true, false, 0, 24);

        assert!(line.contains("bash@local"));
        assert!(line.contains("🍳R"));
    }

    #[test]
    fn sidebar_running_badge_uses_running_color() {
        let mut registry = SessionRegistry::new();
        let session = registry.create_local_session(
            "local".to_string(),
            "bash".to_string(),
            "/bin/bash".to_string(),
        );

        let line = format_sidebar_item(&session, false, false, 0, 24);

        assert!(line.contains(ANSI_FG_SIDEBAR_RUNNING));
        assert!(line.contains("🍳R"));
    }

    #[test]
    fn sidebar_input_badge_uses_input_color() {
        let mut registry = SessionRegistry::new();
        let session = registry.create_local_session(
            "local".to_string(),
            "bash".to_string(),
            "/bin/bash".to_string(),
        );

        let line = format_sidebar_item(&session, false, true, 0, 24);

        assert!(line.contains(ANSI_FG_SIDEBAR_INPUT));
        assert!(line.contains("🔊I"));
    }

    #[test]
    fn sidebar_emoji_badge_reports_terminal_display_width() {
        let (_running_badge, running_width) =
            style_sidebar_badge(SidebarDisplayStatus::Running, 0, false);
        let (_input_badge, input_width) =
            style_sidebar_badge(SidebarDisplayStatus::Input, 0, false);
        let (_confirm_badge, confirm_width) =
            style_sidebar_badge(SidebarDisplayStatus::Confirm, 0, false);

        assert_eq!(running_width, 3);
        assert_eq!(input_width, 3);
        assert_eq!(confirm_width, 3);
    }

    #[test]
    fn sidebar_confirm_badge_uses_confirm_color() {
        let mut registry = SessionRegistry::new();
        let session = registry.create_local_session(
            "local".to_string(),
            "codex".to_string(),
            "codex".to_string(),
        );
        let address = session.address().clone();
        let mut engine = TerminalEngine::new(TerminalSize::default());
        engine.feed(b"Approve command? [y/n]");
        registry.update_screen_state(&address, engine.state());
        let record = registry.get(&address).expect("session should exist");

        let line = format_sidebar_item(record, false, true, 0, 24);

        assert!(line.contains(ANSI_FG_SIDEBAR_CONFIRM));
        assert!(line.contains("📢C"));
    }

    #[test]
    fn sidebar_display_status_detects_confirmation_prompt_from_visible_text() {
        let mut registry = SessionRegistry::new();
        let session = registry.create_local_session(
            "local".to_string(),
            "codex".to_string(),
            "codex".to_string(),
        );
        let address = session.address().clone();
        let mut engine = TerminalEngine::new(TerminalSize::default());
        engine.feed(b"Approve command? [y/n]");
        registry.update_screen_state(&address, engine.state());

        let status =
            sidebar_display_status(registry.get(&address).expect("session should exist"), true);

        assert_eq!(status, SidebarDisplayStatus::Confirm);
    }

    #[test]
    fn sidebar_detail_text_truncates_full_label_and_status_without_animation() {
        let mut registry = SessionRegistry::new();
        let session = registry.create_local_session(
            "local".to_string(),
            "very-long-session-name".to_string(),
            "codex".to_string(),
        );
        let address = session.address().clone();
        let mut engine = TerminalEngine::new(TerminalSize::default());
        engine.feed(b"\x1b]0;/opt/data/workspace/wait-agent/projects/demo\x07");
        registry.update_screen_state(&address, engine.state());
        let record = registry.get(&address).expect("session should exist");

        let detail = build_sidebar_detail_source(record, false);
        let first = build_sidebar_detail_text(&detail, 18);
        let second = build_sidebar_detail_text(&detail, 18);

        assert_eq!(first.chars().count(), 18);
        assert_eq!(second.chars().count(), 18);
        assert_eq!(first, second);
    }

    #[test]
    fn sidebar_detail_text_right_aligns_within_available_width() {
        let detail = build_sidebar_detail_text("RUNNING", 10);

        assert_eq!(detail, "   RUNNING");
    }

    #[test]
    fn sidebar_diff_frame_keeps_main_prompt_visible_when_fed_back_into_terminal_engine() {
        let frame_text = [
            "k@k:/opt/data/workspace/wait-agent$ ",
            "",
            "WaitAgent | bash | local/session-1                      active | 0 waiting | 1/1",
        ]
        .join("\r\n");
        let sidebar = SidebarOverlay {
            separator_col: 52,
            content_col: 53,
            divider: style_sidebar_divider(),
            lines: vec![
                style_sidebar_header_line(" Sessions  [h] hide", 28),
                style_sidebar_item_line("> bash@local             🔊I", 28, true),
            ],
        };
        let buffer = build_full_frame_buffer_with_sidebar_diff(
            &frame_text,
            None,
            Some(&sidebar),
            None,
            CursorPlacement { row: 0, col: 36 },
            true,
            24,
        );
        let mut engine = TerminalEngine::new(TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        });
        let wrapped = format!("{ANSI_SYNC_UPDATE_START}{buffer}{ANSI_SYNC_UPDATE_END}");

        engine.feed(wrapped.as_bytes());
        let snapshot = engine.snapshot();

        assert!(
            snapshot.lines[0].starts_with("k@k:/opt/data/workspace/wait-agent$"),
            "row1={:?}",
            snapshot.lines[0]
        );
    }

    #[test]
    fn background_wait_notice_reports_new_non_focused_waiter() {
        let focused = SessionAddress::new("local", "session-1");
        let waiter = SessionAddress::new("local", "session-2");

        let notice = background_wait_notice(
            &[focused.clone()],
            &[focused.clone(), waiter.clone()],
            Some(&focused),
        );

        assert_eq!(
            notice,
            Some(format!("{waiter} is waiting. Press Enter to hand off."))
        );
    }

    #[test]
    fn background_wait_notice_ignores_existing_and_focused_waiters() {
        let focused = SessionAddress::new("local", "session-1");
        let waiter = SessionAddress::new("local", "session-2");

        assert_eq!(
            background_wait_notice(&[focused.clone()], &[focused.clone()], Some(&focused)),
            None
        );
        assert_eq!(
            background_wait_notice(&[focused.clone(), waiter.clone()], &[focused, waiter], None),
            None
        );
    }

    #[test]
    fn input_tracker_ignores_navigation_sequences_for_switch_lock() {
        let mut tracker = InputTracker::default();
        let mut console = ConsoleState::new("console-1");
        console.focus(SessionAddress::new("local", "session-1"));
        let mut scheduler = SchedulerState::new();

        tracker.observe(b"\x1b[A\t", &mut console, &mut scheduler, 100);

        assert!(console.can_switch());
    }

    #[test]
    fn picker_enter_can_focus_after_forwarded_navigation_input() {
        let mut registry = SessionRegistry::new();
        let first = registry.create_local_session(
            "local".to_string(),
            "bash".to_string(),
            "bash".to_string(),
        );
        let second = registry.create_local_session(
            "local".to_string(),
            "codex".to_string(),
            "codex".to_string(),
        );
        let sessions = registry.list();
        let addresses = sessions
            .iter()
            .map(|session| session.address().clone())
            .collect::<Vec<_>>();

        let mut tracker = InputTracker::default();
        let mut console = ConsoleState::new("console-1");
        console.focus(first.address().clone());
        let mut scheduler = SchedulerState::new();
        tracker.observe(b"\x1b[A\t", &mut console, &mut scheduler, 100);

        let mut prompt = CommandPromptState::default();
        prompt.toggle_sessions(&sessions, console.focused_session.as_ref());
        prompt.handle_picker_navigation(
            b"\x1b[B",
            &sessions,
            console.focused_session.as_ref(),
            120,
        );

        let index = prompt
            .selected_picker_index(&sessions, console.focused_session.as_ref())
            .expect("picker selection");
        assert_eq!(index, 2);
        assert_eq!(
            console.focus_index(&addresses, index),
            Some(second.address().clone())
        );
    }

    #[test]
    fn forward_input_normalizer_translates_cursor_keys_for_application_mode() {
        let mut normalizer = ForwardInputNormalizer::default();

        assert_eq!(normalizer.normalize(b"\x1b[A", true, 100), b"\x1bOA");
        assert_eq!(normalizer.normalize(b"\x1b[B", true, 110), b"\x1bOB");
        assert_eq!(normalizer.normalize(b"\x1b[D", true, 120), b"\x1bOD");
        assert_eq!(normalizer.normalize(b"\x1b[H", true, 130), b"\x1bOH");
    }

    #[test]
    fn forward_input_normalizer_keeps_shell_sequences_when_application_mode_is_off() {
        let mut normalizer = ForwardInputNormalizer::default();

        assert_eq!(normalizer.normalize(b"\x1b[A", false, 100), b"\x1b[A");
        assert_eq!(normalizer.normalize(b"\x1b[Z", true, 110), b"\x1b[Z");
    }

    #[test]
    fn forward_input_normalizer_handles_split_arrow_sequences() {
        let mut normalizer = ForwardInputNormalizer::default();

        assert!(normalizer.normalize(&[0x1b], true, 100).is_empty());
        assert!(normalizer.normalize(b"[", true, 110).is_empty());
        assert_eq!(normalizer.normalize(b"A", true, 120), b"\x1bOA");
    }

    #[test]
    fn forward_input_normalizer_flushes_lone_escape_after_timeout() {
        let mut normalizer = ForwardInputNormalizer::default();

        assert!(normalizer.normalize(&[0x1b], true, 100).is_empty());
        assert!(normalizer
            .flush_pending_escape_timeout(100 + PICKER_ESCAPE_TIMEOUT_MS - 1)
            .is_empty());
        assert_eq!(
            normalizer.flush_pending_escape_timeout(100 + PICKER_ESCAPE_TIMEOUT_MS + 1),
            b"\x1b"
        );
    }

    #[test]
    fn live_surface_always_reserves_footer_rows() {
        let terminal_size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        };

        assert_eq!(
            live_surface_target_size(true, false, terminal_size, false),
            TerminalSize {
                rows: 21,
                cols: 50,
                pixel_width: 0,
                pixel_height: 0,
            }
        );
        assert_eq!(
            live_surface_target_size(false, true, terminal_size, false),
            TerminalSize {
                rows: 21,
                cols: 50,
                pixel_width: 0,
                pixel_height: 0,
            }
        );
        assert_eq!(
            live_surface_target_size(false, false, terminal_size, false),
            TerminalSize {
                rows: 21,
                cols: 50,
                pixel_width: 0,
                pixel_height: 0,
            }
        );
        assert_eq!(
            live_surface_target_size(false, false, terminal_size, true),
            TerminalSize {
                rows: 21,
                cols: 76,
                pixel_width: 0,
                pixel_height: 0,
            }
        );
    }

    #[test]
    fn footer_separator_line_labels_the_menu_region() {
        let line = style_footer_separator_line(24);

        assert!(line.contains("━━━━━━━━━━━━━━━━━━━━━━━━"));
        assert!(!line.contains("MENU"));
    }

    #[test]
    fn full_frame_buffer_clears_stale_rows_below_shorter_replacement_frame() {
        let buffer = build_full_frame_buffer(
            "session-2\r\nprompt$ ",
            CursorPlacement { row: 1, col: 7 },
            true,
            4,
        );

        assert!(buffer.starts_with("\x1b[H\x1b[?25l"));
        assert!(buffer.contains("\x1b[1;1H\x1b[K\x1b[1;1Hsession-2"));
        assert!(buffer.contains("\x1b[2;1H\x1b[K\x1b[2;1Hprompt$ "));
        assert!(buffer.contains("\x1b[3;1H\x1b[K"));
        assert!(buffer.contains("\x1b[4;1H\x1b[K"));
        assert!(buffer.ends_with("\x1b[2;8H\x1b[?25h"));
    }

    #[test]
    fn full_frame_buffer_does_not_clear_past_terminal_bottom() {
        let buffer = build_full_frame_buffer(
            "line 1\r\nline 2\r\nline 3",
            CursorPlacement { row: 2, col: 6 },
            true,
            3,
        );

        assert!(buffer.contains("\x1b[1;1H\x1b[K\x1b[1;1Hline 1"));
        assert!(buffer.contains("\x1b[2;1H\x1b[K\x1b[2;1Hline 2"));
        assert!(buffer.contains("\x1b[3;1H\x1b[K\x1b[3;1Hline 3"));
        assert!(!buffer.contains("\x1b[4;1H"));
        assert!(!buffer.contains("\x1b[J"));
        assert!(buffer.ends_with("\x1b[3;7H\x1b[?25h"));
    }

    #[test]
    fn full_frame_buffer_positions_sidebar_without_extending_main_line_text() {
        let sidebar = SidebarOverlay {
            separator_col: 8,
            content_col: 9,
            divider: style_sidebar_divider(),
            lines: vec![
                style_sidebar_header_line("menu", 4),
                style_sidebar_item_line("item", 4, true),
            ],
        };

        let buffer = build_full_frame_buffer_with_sidebar(
            "left\r\nbody",
            Some(&sidebar),
            CursorPlacement { row: 1, col: 2 },
            true,
            3,
        );

        assert!(buffer.contains("\x1b[1;1Hleft\x1b[0m   "));
        assert!(buffer.contains("\x1b[1;8H"));
        assert!(buffer.contains("\x1b[1;9H"));
        assert!(buffer.contains("\x1b[2;1Hbody\x1b[0m   "));
        assert!(buffer.contains("\x1b[2;8H"));
        assert!(buffer.ends_with("\x1b[2;3H\x1b[?25h"));
    }

    #[test]
    fn full_frame_buffer_does_not_clear_entire_sidebar_rows_before_redraw() {
        let sidebar = SidebarOverlay {
            separator_col: 8,
            content_col: 9,
            divider: style_sidebar_divider(),
            lines: vec![
                style_sidebar_header_line("menu", 4),
                style_sidebar_item_line("item", 4, true),
            ],
        };

        let buffer = build_full_frame_buffer_with_sidebar(
            "left\r\nbody",
            Some(&sidebar),
            CursorPlacement { row: 1, col: 2 },
            true,
            3,
        );

        assert!(!buffer.contains("\x1b[1;1Hleft\x1b[K"));
        assert!(!buffer.contains("\x1b[2;1Hbody\x1b[K"));
        assert!(!buffer.contains("\x1b[1;1H\x1b[0m       \x1b[1;1H"));
        assert!(!buffer.contains("\x1b[2;1H\x1b[0m       \x1b[2;1H"));
        assert!(buffer.contains("\x1b[1;1Hleft\x1b[0m   "));
        assert!(buffer.contains("\x1b[2;1Hbody\x1b[0m   "));
        assert!(buffer.contains("\x1b[1;8H"));
        assert!(buffer.contains("\x1b[2;8H"));
    }

    #[test]
    fn sidebar_overlay_buffer_skips_unchanged_rows_for_focus_only_updates() {
        let sidebar = SidebarOverlay {
            separator_col: 8,
            content_col: 9,
            divider: style_sidebar_divider(),
            lines: vec![
                style_sidebar_header_line("menu", 4),
                style_sidebar_item_line("item", 4, true),
            ],
        };

        let buffer = crate::app::build_sidebar_overlay_buffer(
            &sidebar,
            Some(&sidebar),
            CursorPlacement { row: 1, col: 2 },
            "\x1b[0m",
            false,
        );

        assert_eq!(buffer, "\x1b[?25l\x1b[2;3H\x1b[0m\x1b[?25l");
    }

    #[test]
    fn sidebar_overlay_buffer_updates_only_changed_rows() {
        let previous = SidebarOverlay {
            separator_col: 8,
            content_col: 9,
            divider: style_sidebar_divider(),
            lines: vec![
                style_sidebar_header_line("menu", 4),
                style_sidebar_item_line("item", 4, true),
                crate::app::style_sidebar_detail_line("one", 4),
            ],
        };
        let current = SidebarOverlay {
            separator_col: 8,
            content_col: 9,
            divider: style_sidebar_divider(),
            lines: vec![
                style_sidebar_header_line("menu", 4),
                style_sidebar_item_line("next", 4, false),
                crate::app::style_sidebar_detail_line("two", 4),
            ],
        };

        let buffer = crate::app::build_sidebar_overlay_buffer(
            &current,
            Some(&previous),
            CursorPlacement { row: 1, col: 2 },
            "\x1b[0m",
            true,
        );

        assert!(!buffer.contains("\x1b[1;8H"));
        assert!(buffer.contains("\x1b[2;8H"));
        assert!(buffer.contains("\x1b[3;8H"));
        assert!(buffer.starts_with("\x1b[?25l"));
        assert!(buffer.ends_with("\x1b[2;3H\x1b[0m\x1b[?25h"));
    }

    #[test]
    fn sidebar_overlay_buffer_clears_trailing_safe_column_with_sidebar_style() {
        let sidebar = SidebarOverlay {
            separator_col: 8,
            content_col: 9,
            divider: style_sidebar_divider(),
            lines: vec![style_sidebar_item_line("item", 4, true)],
        };

        let buffer = crate::app::build_sidebar_overlay_buffer(
            &sidebar,
            None,
            CursorPlacement { row: 1, col: 2 },
            "\x1b[0m",
            false,
        );

        assert!(buffer.contains(&format!("\x1b[1;9H{ANSI_BG_SIDEBAR_ACTIVE}\x1b[K")));
    }

    #[test]
    fn sidebar_overlay_buffer_restores_cursor_position_and_style() {
        let sidebar = SidebarOverlay {
            separator_col: 8,
            content_col: 9,
            divider: style_sidebar_divider(),
            lines: vec![
                style_sidebar_header_line("menu", 4),
                style_sidebar_item_line("item", 4, true),
            ],
        };
        let buffer = crate::app::build_sidebar_overlay_buffer(
            &sidebar,
            None,
            CursorPlacement { row: 0, col: 3 },
            "\x1b[0;38;5;196m",
            true,
        );
        let mut terminal = TerminalEngine::new(TerminalSize::default());

        terminal.feed(b"\x1b[38;5;196mred");
        terminal.feed(buffer.as_bytes());
        let snapshot = terminal.snapshot();

        assert_eq!(snapshot.cursor_row, 0);
        assert_eq!(snapshot.cursor_col, 3);
        assert_eq!(snapshot.active_style_ansi, "\x1b[0;38;5;196m");
        assert!(snapshot.cursor_visible);
    }

    #[test]
    fn full_frame_buffer_does_not_draw_sidebar_on_bottom_status_row() {
        let sidebar = SidebarOverlay {
            separator_col: 8,
            content_col: 9,
            divider: style_sidebar_divider(),
            lines: vec![style_sidebar_header_line("menu", 4)],
        };

        let buffer = build_full_frame_buffer_with_sidebar(
            "line 1\r\nstatus",
            Some(&sidebar),
            CursorPlacement { row: 1, col: 1 },
            true,
            2,
        );

        assert!(buffer.contains("\x1b[1;8H"));
        assert!(!buffer.contains("\x1b[2;8H"));
        assert!(!buffer.contains("\x1b[2;9H"));
    }

    #[test]
    fn full_frame_buffer_diff_skips_unchanged_sidebar_rows() {
        let sidebar = SidebarOverlay {
            separator_col: 8,
            content_col: 9,
            divider: style_sidebar_divider(),
            lines: vec![
                style_sidebar_header_line("menu", 4),
                style_sidebar_item_line("item", 4, true),
            ],
        };

        let buffer = crate::app::build_full_frame_buffer_with_sidebar_diff(
            "left\r\nbody",
            None,
            Some(&sidebar),
            Some(&sidebar),
            CursorPlacement { row: 1, col: 2 },
            true,
            2,
        );

        assert!(buffer.starts_with("\x1b[H\x1b[?25l"));
        assert!(!buffer.contains("\x1b[1;8H"));
        assert!(!buffer.contains("\x1b[2;8H"));
        assert!(buffer.ends_with("\x1b[2;3H\x1b[?25h"));
    }

    #[test]
    fn full_frame_buffer_diff_treats_empty_previous_frame_as_full_redraw() {
        let previous: Vec<String> = Vec::new();

        let buffer = crate::app::build_full_frame_buffer_with_sidebar_diff(
            "next session",
            Some(previous.as_slice()),
            None,
            None,
            CursorPlacement { row: 0, col: 0 },
            true,
            4,
        );

        assert!(buffer.contains("\x1b[2;1H\x1b[K"));
        assert!(buffer.contains("\x1b[3;1H\x1b[K"));
        assert!(buffer.contains("\x1b[4;1H\x1b[K"));
    }

    #[test]
    fn full_frame_buffer_diff_clears_empty_main_rows_when_previous_frame_is_unknown() {
        let sidebar = SidebarOverlay {
            separator_col: 8,
            content_col: 9,
            divider: style_sidebar_divider(),
            lines: vec![
                style_sidebar_header_line("menu", 4),
                style_sidebar_item_line("item", 4, true),
                style_sidebar_item_line("", 4, false),
            ],
        };

        let buffer = crate::app::build_full_frame_buffer_with_sidebar_diff(
            "prompt$ \r\n\r\nstatus",
            None,
            Some(&sidebar),
            None,
            CursorPlacement { row: 0, col: 7 },
            true,
            3,
        );

        assert!(buffer.contains("\x1b[2;1H\x1b[0m       "));
        assert!(!buffer.contains("\x1b[2;1H\x1b[0m       \x1b[2;1H"));
    }

    #[test]
    fn live_surface_stays_off_for_plain_shell_state() {
        let mut app = App::new(AppConfig::from_env());
        let session = app.sessions.create_local_session(
            "local".to_string(),
            "bash".to_string(),
            "bash".to_string(),
        );
        let address = session.address().clone();
        let mut console = ConsoleState::new("console-1");
        console.focus(address.clone());

        let mut engine = TerminalEngine::new(TerminalSize::default());
        engine.feed(b"prompt$ ");
        app.sessions.update_screen_state(&address, engine.state());

        assert!(!app.focused_session_supports_live_surface(&console));
    }

    #[test]
    fn live_surface_turns_on_for_tui_like_state() {
        let mut app = App::new(AppConfig::from_env());
        let session = app.sessions.create_local_session(
            "local".to_string(),
            "codex".to_string(),
            "codex".to_string(),
        );
        let address = session.address().clone();
        let mut console = ConsoleState::new("console-1");
        console.focus(address.clone());

        let mut engine = TerminalEngine::new(TerminalSize::default());
        engine.feed(b"\x1b[?1049h");
        app.sessions.update_screen_state(&address, engine.state());

        assert!(app.focused_session_supports_live_surface(&console));
    }

    #[test]
    fn live_surface_stays_off_for_application_cursor_without_alt_screen() {
        let mut app = App::new(AppConfig::from_env());
        let session = app.sessions.create_local_session(
            "local".to_string(),
            "codex".to_string(),
            "codex".to_string(),
        );
        let address = session.address().clone();
        let mut console = ConsoleState::new("console-1");
        console.focus(address.clone());

        let mut engine = TerminalEngine::new(TerminalSize::default());
        engine.feed(b"\x1b[?1h");
        app.sessions.update_screen_state(&address, engine.state());

        assert!(!app.focused_session_supports_live_surface(&console));
    }

    #[test]
    fn known_live_command_keeps_live_surface_without_alt_screen_state() {
        let mut app = App::new(AppConfig::from_env());
        let session = app.sessions.create_local_session(
            "local".to_string(),
            "codex".to_string(),
            "codex".to_string(),
        );
        let address = session.address().clone();
        let mut console = ConsoleState::new("console-1");
        console.focus(address.clone());
        let command_prompt = CommandPromptState::default();
        let sidebar = SidebarState::default();
        let mut live_surface = LiveSurfaceState::default();
        let mut hosted = HashMap::new();

        let mut engine = TerminalEngine::new(TerminalSize::default());
        engine.feed(b"OpenAI Codex\r\nsession menu");
        app.sessions.update_screen_state(&address, engine.state());
        live_surface.mark_known_live_command(address.clone());
        live_surface.set_display_session(Some(address.clone()), true, 100);
        live_surface.pending_redraw = false;

        assert!(app.focused_session_prefers_live_surface(&live_surface, &console));
        assert!(!app
            .maybe_deactivate_live_surface_after_output(
                &mut live_surface,
                &mut hosted,
                &console,
                &command_prompt,
                &sidebar,
                &address,
            )
            .expect("known live command should remain active"));
    }

    #[test]
    fn bootstrapping_live_command_activates_live_surface_before_takeover_sequences() {
        let mut app = App::new(AppConfig::from_env());
        let session = app.sessions.create_local_session(
            "local".to_string(),
            "codex".to_string(),
            "codex".to_string(),
        );
        let address = session.address().clone();
        let mut console = ConsoleState::new("console-1");
        console.focus(address.clone());
        let command_prompt = CommandPromptState::default();
        let sidebar = SidebarState::default();
        let mut live_surface = LiveSurfaceState::default();
        let mut hosted = HashMap::new();

        live_surface.mark_known_live_command(address.clone());
        live_surface.mark_session_bootstrapping(address.clone(), now_unix_ms());

        assert!(!app
            .maybe_activate_live_surface_for_output(
                &mut live_surface,
                &mut hosted,
                &console,
                &command_prompt,
                &sidebar,
                &address,
                b"OpenAI Codex\r\n",
            )
            .expect("plain startup banner should not require takeover markers"));
        assert!(live_surface.is_live_for(&address));
        assert!(app.focused_session_owns_passthrough_display(&live_surface, &console));
    }

    #[test]
    fn live_surface_does_not_stick_after_session_returns_to_shell_mode() {
        let mut app = App::new(AppConfig::from_env());
        let session = app.sessions.create_local_session(
            "local".to_string(),
            "codex".to_string(),
            "codex".to_string(),
        );
        let address = session.address().clone();
        let mut console = ConsoleState::new("console-1");
        console.focus(address.clone());
        let live_surface = LiveSurfaceState::default();

        let mut fullscreen = TerminalEngine::new(TerminalSize::default());
        fullscreen.feed(b"\x1b[?1049h");
        app.sessions
            .update_screen_state(&address, fullscreen.state());
        assert!(app.focused_session_prefers_live_surface(&live_surface, &console));

        let mut shell = TerminalEngine::new(TerminalSize::default());
        shell.feed(b"prompt$ ");
        app.sessions.update_screen_state(&address, shell.state());
        assert!(!app.focused_session_prefers_live_surface(&live_surface, &console));
    }

    #[test]
    fn live_surface_deactivates_immediately_after_output_returns_to_shell_mode() {
        let mut app = App::new(AppConfig::from_env());
        let session = app.sessions.create_local_session(
            "local".to_string(),
            "codex".to_string(),
            "codex".to_string(),
        );
        let address = session.address().clone();
        let mut console = ConsoleState::new("console-1");
        console.focus(address.clone());
        let command_prompt = CommandPromptState::default();
        let sidebar = SidebarState::default();
        let mut live_surface = LiveSurfaceState::default();
        let mut hosted = HashMap::new();

        let mut fullscreen = TerminalEngine::new(TerminalSize::default());
        fullscreen.feed(b"\x1b[?1049hHELLO");
        app.sessions
            .update_screen_state(&address, fullscreen.state());
        live_surface.set_display_session(Some(address.clone()), true, 100);
        live_surface.pending_redraw = false;

        let mut shell = TerminalEngine::new(TerminalSize::default());
        shell.feed(b"prompt$ ");
        app.sessions.update_screen_state(&address, shell.state());

        assert!(app
            .maybe_deactivate_live_surface_after_output(
                &mut live_surface,
                &mut hosted,
                &console,
                &command_prompt,
                &sidebar,
                &address,
            )
            .expect("live surface deactivation should succeed"));
        assert!(!app.focused_session_owns_passthrough_display(&live_surface, &console));
    }

    #[test]
    fn live_surface_stays_active_while_output_remains_in_alternate_screen() {
        let mut app = App::new(AppConfig::from_env());
        let session = app.sessions.create_local_session(
            "local".to_string(),
            "codex".to_string(),
            "codex".to_string(),
        );
        let address = session.address().clone();
        let mut console = ConsoleState::new("console-1");
        console.focus(address.clone());
        let command_prompt = CommandPromptState::default();
        let sidebar = SidebarState::default();
        let mut live_surface = LiveSurfaceState::default();
        let mut hosted = HashMap::new();

        let mut fullscreen = TerminalEngine::new(TerminalSize::default());
        fullscreen.feed(b"\x1b[?1049hHELLO");
        app.sessions
            .update_screen_state(&address, fullscreen.state());
        live_surface.set_display_session(Some(address.clone()), true, 100);
        live_surface.pending_redraw = false;

        assert!(!app
            .maybe_deactivate_live_surface_after_output(
                &mut live_surface,
                &mut hosted,
                &console,
                &command_prompt,
                &sidebar,
                &address,
            )
            .expect("alternate-screen output should remain live"));
        assert!(app.focused_session_owns_passthrough_display(&live_surface, &console));
    }

    #[test]
    fn live_surface_deactivates_after_terminal_release_even_if_app_cursor_lingers() {
        let mut app = App::new(AppConfig::from_env());
        let session = app.sessions.create_local_session(
            "local".to_string(),
            "codex".to_string(),
            "codex".to_string(),
        );
        let address = session.address().clone();
        let mut console = ConsoleState::new("console-1");
        console.focus(address.clone());
        let command_prompt = CommandPromptState::default();
        let sidebar = SidebarState::default();
        let mut live_surface = LiveSurfaceState::default();
        let mut hosted = HashMap::new();

        let mut fullscreen = TerminalEngine::new(TerminalSize::default());
        fullscreen.feed(b"\x1b[?1h\x1b[?1049hHELLO");
        app.sessions
            .update_screen_state(&address, fullscreen.state());
        live_surface.mark_known_live_command(address.clone());
        live_surface.mark_session_bootstrapping(address.clone(), now_unix_ms());
        live_surface.set_display_session(Some(address.clone()), true, 100);
        live_surface.pending_redraw = false;

        let mut shell = TerminalEngine::new(TerminalSize::default());
        shell.feed(b"\x1b[?1h\x1b[?1049lprompt$ ");
        app.sessions.update_screen_state(&address, shell.state());
        live_surface.clear_known_live_command(&address);
        live_surface.clear_session_bootstrapping(&address);

        assert!(!app.focused_session_prefers_live_surface(&live_surface, &console));
        assert!(app
            .maybe_deactivate_live_surface_after_output(
                &mut live_surface,
                &mut hosted,
                &console,
                &command_prompt,
                &sidebar,
                &address,
            )
            .expect("terminal release should deactivate live surface"));
        assert!(!app.focused_session_owns_passthrough_display(&live_surface, &console));
    }

    #[test]
    fn background_tui_session_keeps_fullscreen_preference() {
        let mut app = App::new(AppConfig::from_env());
        let shell = app.sessions.create_local_session(
            "local".to_string(),
            "bash".to_string(),
            "bash".to_string(),
        );
        let codex = app.sessions.create_local_session(
            "local".to_string(),
            "codex".to_string(),
            "codex".to_string(),
        );

        let shell_address = shell.address().clone();
        let codex_address = codex.address().clone();
        let mut shell_engine = TerminalEngine::new(TerminalSize::default());
        shell_engine.feed(b"prompt$ ");
        app.sessions
            .update_screen_state(&shell_address, shell_engine.state());

        let mut codex_engine = TerminalEngine::new(TerminalSize::default());
        codex_engine.feed(b"\x1b[?1049h");
        app.sessions
            .update_screen_state(&codex_address, codex_engine.state());

        let live_surface = LiveSurfaceState::default();
        assert!(!app.session_prefers_fullscreen_background(&live_surface, &shell_address));
        assert!(app.session_prefers_fullscreen_background(&live_surface, &codex_address));
    }

    #[test]
    fn live_surface_overlay_restores_cursor_and_active_style_after_drawing_footer() {
        let mut app = App::new(AppConfig::from_env());
        let session = app.sessions.create_local_session(
            "local".to_string(),
            "codex".to_string(),
            "codex".to_string(),
        );
        let address = session.address().clone();
        let mut console = ConsoleState::new("console-1");
        console.focus(address.clone());
        let command_prompt = CommandPromptState::default();
        let scheduler = SchedulerState::new();
        let renderer = Renderer::new();
        let mut renderer_state = RendererState::default();
        let mut sidebar = SidebarState::default();

        let mut engine = TerminalEngine::new(TerminalSize::default());
        engine.feed(b"\x1b[38;5;196mred");
        app.sessions.update_screen_state(&address, engine.state());

        let ui = app
            .build_live_surface_ui_buffer(
                &LiveSurfaceState::default(),
                &command_prompt,
                &mut renderer_state,
                &renderer,
                &console,
                &scheduler,
                &mut sidebar,
                0,
                true,
                None,
            )
            .expect("overlay buffer should build");
        let buffer = ui.buffer;
        let mut terminal = TerminalEngine::new(TerminalSize::default());
        terminal.feed(b"\x1b[38;5;196mred");
        terminal.feed(buffer.as_bytes());
        let snapshot = terminal.snapshot();

        assert!(buffer.contains("Sessions  [h] hide"));
        assert!(!buffer.contains("\x1b[s"));
        assert!(!buffer.contains("\x1b[u"));
        assert_eq!(snapshot.cursor_row, 0);
        assert_eq!(snapshot.cursor_col, 3);
        assert_eq!(snapshot.active_style_ansi, "\x1b[0;38;5;196m");
    }

    #[test]
    fn live_surface_overlay_hides_cursor_while_bootstrapping() {
        let mut app = App::new(AppConfig::from_env());
        let session = app.sessions.create_local_session(
            "local".to_string(),
            "codex".to_string(),
            "/bin/bash".to_string(),
        );
        let address = session.address().clone();
        let mut console = ConsoleState::new("console-1");
        console.focus(address.clone());
        let command_prompt = CommandPromptState::default();
        let scheduler = SchedulerState::new();
        let renderer = Renderer::new();
        let mut renderer_state = RendererState::default();
        let mut sidebar = SidebarState::default();
        let mut live_surface = LiveSurfaceState::default();
        live_surface.mark_known_live_command(address.clone());
        live_surface.mark_session_bootstrapping(address.clone(), now_unix_ms());

        let mut engine = TerminalEngine::new(TerminalSize::default());
        engine.feed(b"\x1b[38;5;196mred");
        app.sessions.update_screen_state(&address, engine.state());

        let ui = app
            .build_live_surface_ui_buffer(
                &live_surface,
                &command_prompt,
                &mut renderer_state,
                &renderer,
                &console,
                &scheduler,
                &mut sidebar,
                0,
                true,
                None,
            )
            .expect("overlay buffer should build");
        let buffer = ui.buffer;

        assert!(buffer.contains("Sessions  [h] hide"));
        assert!(!buffer.contains("\x1b[?25h"));
        assert!(!buffer.contains("\x1b[?25l"));
    }

    #[test]
    fn live_surface_overlay_skips_unchanged_sidebar_rows_when_not_forced() {
        let mut app = App::new(AppConfig::from_env());
        let session = app.sessions.create_local_session(
            "local".to_string(),
            "codex".to_string(),
            "codex".to_string(),
        );
        let address = session.address().clone();
        let mut console = ConsoleState::new("console-1");
        console.focus(address.clone());
        let command_prompt = CommandPromptState::default();
        let scheduler = SchedulerState::new();
        let renderer = Renderer::new();
        let mut renderer_state = RendererState::default();
        let mut sidebar = SidebarState::default();
        let mut live_surface = LiveSurfaceState::default();

        let mut engine = TerminalEngine::new(TerminalSize::default());
        engine.feed(b"\x1b[?1049hcodex");
        app.sessions.update_screen_state(&address, engine.state());

        let initial_ui = app
            .build_live_surface_ui_buffer(
                &live_surface,
                &command_prompt,
                &mut renderer_state,
                &renderer,
                &console,
                &scheduler,
                &mut sidebar,
                0,
                true,
                None,
            )
            .expect("initial overlay should build");
        live_surface.chrome_visible = true;
        live_surface.overlay_rows = initial_ui.overlay_rows;
        live_surface.sidebar_overlay = initial_ui.sidebar_overlay;
        live_surface.separator_line = initial_ui.separator_line;
        live_surface.keys_line = initial_ui.keys_line;
        live_surface.status_line = initial_ui.status_line;

        let ui = app
            .build_live_surface_ui_buffer(
                &live_surface,
                &command_prompt,
                &mut renderer_state,
                &renderer,
                &console,
                &scheduler,
                &mut sidebar,
                0,
                false,
                None,
            )
            .expect("follow-up overlay should build");
        let buffer = ui.buffer;

        assert!(!buffer.contains("Sessions  [h] hide"));
        assert!(!buffer.contains("> codex"));
    }

    #[test]
    fn live_surface_overlay_redraws_sidebar_rows_marked_as_damaged() {
        let mut app = App::new(AppConfig::from_env());
        let session = app.sessions.create_local_session(
            "local".to_string(),
            "codex".to_string(),
            "codex".to_string(),
        );
        let address = session.address().clone();
        let mut console = ConsoleState::new("console-1");
        console.focus(address.clone());
        let command_prompt = CommandPromptState::default();
        let scheduler = SchedulerState::new();
        let renderer = Renderer::new();
        let mut renderer_state = RendererState::default();
        let mut sidebar = SidebarState::default();
        let mut live_surface = LiveSurfaceState::default();

        let mut engine = TerminalEngine::new(TerminalSize::default());
        engine.feed(b"\x1b[?1049hcodex");
        app.sessions.update_screen_state(&address, engine.state());

        let initial_ui = app
            .build_live_surface_ui_buffer(
                &live_surface,
                &command_prompt,
                &mut renderer_state,
                &renderer,
                &console,
                &scheduler,
                &mut sidebar,
                0,
                true,
                None,
            )
            .expect("initial overlay should build");
        live_surface.chrome_visible = true;
        live_surface.overlay_rows = initial_ui.overlay_rows;
        live_surface.sidebar_overlay = initial_ui.sidebar_overlay;
        live_surface.separator_line = initial_ui.separator_line;
        live_surface.keys_line = initial_ui.keys_line;
        live_surface.status_line = initial_ui.status_line;

        let ui = app
            .build_live_surface_ui_buffer(
                &live_surface,
                &command_prompt,
                &mut renderer_state,
                &renderer,
                &console,
                &scheduler,
                &mut sidebar,
                0,
                false,
                Some((1, usize::MAX)),
            )
            .expect("damaged overlay should build");
        let buffer = ui.buffer;

        assert!(buffer.contains("Sessions  [h] hide"));
        assert!(buffer.contains("> codex"));
    }

    #[test]
    fn live_surface_overlay_forces_sidebar_redraw_after_full_clear() {
        let mut app = App::new(AppConfig::from_env());
        let session = app.sessions.create_local_session(
            "local".to_string(),
            "codex".to_string(),
            "codex".to_string(),
        );
        let address = session.address().clone();
        let mut console = ConsoleState::new("console-1");
        console.focus(address.clone());
        let command_prompt = CommandPromptState::default();
        let scheduler = SchedulerState::new();
        let renderer = Renderer::new();
        let mut renderer_state = RendererState::default();
        let mut sidebar = SidebarState::default();
        let mut live_surface = LiveSurfaceState::default();

        let mut engine = TerminalEngine::new(TerminalSize::default());
        engine.feed(b"\x1b[?1049hcodex");
        app.sessions.update_screen_state(&address, engine.state());

        let initial_ui = app
            .build_live_surface_ui_buffer(
                &live_surface,
                &command_prompt,
                &mut renderer_state,
                &renderer,
                &console,
                &scheduler,
                &mut sidebar,
                0,
                true,
                None,
            )
            .expect("initial overlay should build");
        live_surface.chrome_visible = true;
        live_surface.overlay_rows = initial_ui.overlay_rows;
        live_surface.sidebar_overlay = initial_ui.sidebar_overlay;
        live_surface.separator_line = initial_ui.separator_line;
        live_surface.keys_line = initial_ui.keys_line;
        live_surface.status_line = initial_ui.status_line;

        let ui = app
            .build_live_surface_ui_buffer(
                &live_surface,
                &command_prompt,
                &mut renderer_state,
                &renderer,
                &console,
                &scheduler,
                &mut sidebar,
                0,
                true,
                None,
            )
            .expect("forced redraw overlay should build");
        let buffer = ui.buffer;

        assert!(buffer.contains("Sessions  [h] hide"));
        assert!(buffer.contains("> codex"));
    }

    #[test]
    fn live_surface_overlay_does_not_clear_separator_with_default_background() {
        let mut app = App::new(AppConfig::from_env());
        let session = app.sessions.create_local_session(
            "local".to_string(),
            "codex".to_string(),
            "codex".to_string(),
        );
        let address = session.address().clone();
        let mut console = ConsoleState::new("console-1");
        console.focus(address.clone());
        let command_prompt = CommandPromptState::default();
        let scheduler = SchedulerState::new();
        let renderer = Renderer::new();
        let mut renderer_state = RendererState::default();
        let mut sidebar = SidebarState::default();

        let mut engine = TerminalEngine::new(TerminalSize::default());
        engine.feed(b"\x1b[?1049hcodex");
        app.sessions.update_screen_state(&address, engine.state());

        let ui = app
            .build_live_surface_ui_buffer(
                &LiveSurfaceState::default(),
                &command_prompt,
                &mut renderer_state,
                &renderer,
                &console,
                &scheduler,
                &mut sidebar,
                0,
                true,
                None,
            )
            .expect("overlay buffer should build");

        let separator_row = app
            .terminal
            .current_size_or_default()
            .rows
            .saturating_sub(LIVE_SURFACE_STATUS_ROWS.saturating_sub(1));
        let separator_write = format!("\x1b[{separator_row};1H{}", ui.separator_line);
        assert!(ui.buffer.contains(&separator_write));
        assert!(!ui.buffer.contains(&format!("{separator_write}\x1b[K")));
    }

    #[test]
    fn live_surface_overlay_draws_keys_row_and_keeps_sidebar_detail() {
        let mut app = App::new(AppConfig::from_env());
        let session = app.sessions.create_local_session(
            "local".to_string(),
            "codex".to_string(),
            "codex".to_string(),
        );
        let address = session.address().clone();
        let mut console = ConsoleState::new("console-1");
        console.focus(address.clone());
        let command_prompt = CommandPromptState::default();
        let scheduler = SchedulerState::new();
        let renderer = Renderer::new();
        let mut renderer_state = RendererState::default();
        let mut sidebar = SidebarState::default();

        let mut engine = TerminalEngine::new(TerminalSize::default());
        engine.feed(b"\x1b[?1049hcodex");
        app.sessions.update_screen_state(&address, engine.state());

        let ui = app
            .build_live_surface_ui_buffer(
                &LiveSurfaceState::default(),
                &command_prompt,
                &mut renderer_state,
                &renderer,
                &console,
                &scheduler,
                &mut sidebar,
                0,
                true,
                None,
            )
            .expect("overlay buffer should build");

        let status_row = app.terminal.current_size_or_default().rows;
        let keys_row = status_row.saturating_sub(1);
        let keys_write = format!("\x1b[{keys_row};1H{}", ui.keys_line);
        assert!(ui.buffer.contains(&keys_write));
        assert!(ui.buffer.contains("codex@local | RUNNING"));
    }

    #[test]
    fn restores_shell_session_title_after_live_command_finishes() {
        let mut app = App::new(AppConfig::from_env());
        let session = app.sessions.create_local_session(
            "local".to_string(),
            "bash".to_string(),
            "/bin/bash".to_string(),
        );
        let address = session.address().clone();

        app.set_session_title(&address, "codex");
        app.restore_shell_session_title(&address);

        assert_eq!(
            app.sessions
                .get(&address)
                .expect("session should exist")
                .title,
            "bash"
        );
    }

    #[test]
    fn live_surface_rearms_redraw_when_same_session_returns_to_active() {
        let session = SessionAddress::new("local", "session-1");
        let mut live_surface = LiveSurfaceState::default();

        live_surface.set_display_session(Some(session.clone()), true, 100);
        assert!(live_surface.pending_redraw);

        live_surface.pending_redraw = false;
        live_surface.set_display_session(Some(session.clone()), false, 110);
        assert!(!live_surface.pending_redraw);

        live_surface.set_display_session(Some(session), true, 120);
        assert!(live_surface.pending_redraw);
    }

    #[test]
    fn live_surface_rearms_redraw_when_passthrough_resumes_after_chrome() {
        let mut live_surface = LiveSurfaceState::default();
        live_surface.chrome_visible = true;
        live_surface.overlay_rows = 2;

        assert!(live_surface.begin_passthrough_output());
        assert!(!live_surface.chrome_visible);
        assert_eq!(live_surface.overlay_rows, 0);
        assert!(live_surface.pending_redraw);
    }

    #[test]
    fn backgrounding_live_surface_clears_chrome_state() {
        let session = SessionAddress::new("local", "session-1");
        let mut live_surface = LiveSurfaceState::default();
        live_surface.chrome_visible = true;
        live_surface.overlay_rows = 2;

        live_surface.set_display_session(Some(session), false, 100);

        assert!(!live_surface.chrome_visible);
        assert_eq!(live_surface.overlay_rows, 0);
    }

    #[test]
    fn leaving_live_surface_requires_full_workspace_redraw() {
        let session = SessionAddress::new("local", "session-1");
        let mut live_surface = LiveSurfaceState::default();

        assert!(!live_surface.display_may_be_live_owned());

        live_surface.set_display_session(Some(session), true, 100);
        assert!(live_surface.display_may_be_live_owned());

        live_surface.set_display_session(None, false, 110);
        assert!(!live_surface.display_may_be_live_owned());
    }

    #[test]
    fn sidebar_only_redraw_stays_off_while_live_surface_cleanup_is_pending() {
        let mut app = App::new(AppConfig::from_env());
        let session = app.sessions.create_local_session(
            "local".to_string(),
            "codex".to_string(),
            "codex".to_string(),
        );
        let address = session.address().clone();
        let mut console = ConsoleState::new("console-1");
        console.focus(address.clone());
        let previous_sidebar = SidebarState::default();
        let sidebar = SidebarState::default();
        let command_prompt = CommandPromptState::default();
        let mut live_surface = LiveSurfaceState::default();

        live_surface.set_display_session(Some(address), false, 100);

        assert!(live_surface.display_may_be_live_owned());
        assert!(!app.focused_session_owns_passthrough_display(&live_surface, &console));
        assert!(!app.can_redraw_sidebar_only(
            &previous_sidebar,
            &sidebar,
            &live_surface,
            &console,
            &command_prompt,
        ));
    }

    #[test]
    fn sidebar_only_redraw_stays_off_when_sidebar_focus_changes() {
        let app = App::new(AppConfig::from_env());
        let previous_sidebar = SidebarState {
            focused: true,
            ..SidebarState::default()
        };
        let sidebar = SidebarState::default();
        let live_surface = LiveSurfaceState::default();
        let console = ConsoleState::new("console-1");
        let command_prompt = CommandPromptState::default();

        assert!(!app.can_redraw_sidebar_only(
            &previous_sidebar,
            &sidebar,
            &live_surface,
            &console,
            &command_prompt,
        ));
    }

    #[test]
    fn sidebar_only_redraw_remains_available_without_live_surface_damage() {
        let app = App::new(AppConfig::from_env());
        let previous_sidebar = SidebarState::default();
        let sidebar = SidebarState::default();
        let live_surface = LiveSurfaceState::default();
        let console = ConsoleState::new("console-1");
        let command_prompt = CommandPromptState::default();

        assert!(app.can_redraw_sidebar_only(
            &previous_sidebar,
            &sidebar,
            &live_surface,
            &console,
            &command_prompt,
        ));
    }

    #[test]
    fn takeover_detection_stays_off_for_plain_shell_output() {
        assert!(!looks_like_terminal_takeover_output(
            b"\x1b[?2004hk@k:/tmp$ "
        ));
        assert!(!looks_like_terminal_takeover_output(b"\x1b[2J"));
    }

    #[test]
    fn live_output_redraws_chrome_only_for_release_or_sidebar_damage() {
        assert!(!live_output_needs_chrome_redraw(b"\x1b[6n"));
        assert!(!live_output_needs_chrome_redraw(b"\x1b[?25l\x1b[16;3H"));
        assert!(live_output_needs_chrome_redraw(b"\x1b[4;1H\x1b[K"));
        assert!(live_output_needs_chrome_redraw(b"\x1b[13;1H\x1b[J"));
        assert!(live_output_needs_chrome_redraw(b"\x1b[?1049lprompt$ "));
        assert!(!live_output_needs_chrome_redraw(
            b"\x1b[?2026h\x1b[1;16r\x1b[1S\x1b[r"
        ));
        assert!(!live_output_needs_chrome_redraw(b"plain output line\r\n"));
    }

    #[test]
    fn live_output_only_forces_full_sidebar_redraw_for_global_damage() {
        assert!(!live_output_requires_full_sidebar_redraw(
            b"\x1b[22;1H\x1b[J"
        ));
        assert!(!live_output_requires_full_sidebar_redraw(
            b"\x1b[?2026h\x1b[1;16r\x1b[1S\x1b[r"
        ));
        assert!(live_output_requires_full_sidebar_redraw(b"\x1b[2J"));
        assert!(live_output_requires_full_sidebar_redraw(
            b"\x1b[?1049lprompt$ "
        ));
    }

    #[test]
    fn live_output_forces_full_sidebar_redraw_when_snapshot_switches_screen_mode() {
        let size = TerminalSize {
            rows: 6,
            cols: 8,
            pixel_width: 0,
            pixel_height: 0,
        };
        let before = TerminalEngine::new(size).state().active_snapshot().clone();
        let mut engine = TerminalEngine::new(size);
        engine.feed(b"\x1b[?1049h");
        let after = engine.state().active_snapshot().clone();

        assert!(live_output_requires_full_sidebar_redraw_from_snapshots(
            Some(&before),
            Some(&after),
        ));
    }

    #[test]
    fn live_output_tracks_sidebar_damage_rows_for_clear_to_end_of_screen() {
        assert_eq!(
            live_output_sidebar_damage(b"\x1b[1;1H\x1b[J"),
            Some((1, usize::MAX))
        );
        assert_eq!(
            live_output_sidebar_damage(b"\x1b[13;1H\x1b[J"),
            Some((13, usize::MAX))
        );
        assert_eq!(
            live_output_sidebar_damage(b"\x1b[13;1H\x1b[1J"),
            Some((1, 13))
        );
        assert_eq!(live_output_sidebar_damage(b"\x1b[4;1H\x1b[K"), Some((4, 4)));
        assert_eq!(
            live_output_sidebar_damage(b"\x1b[2;21r\x1bM\x1bM"),
            Some((2, 21))
        );
        assert_eq!(live_output_sidebar_damage(b"\x1b[6n"), None);
    }

    #[test]
    fn live_surface_snapshot_delta_replays_final_workspace_without_screen_clear_sequences() {
        let size = TerminalSize {
            rows: 6,
            cols: 8,
            pixel_width: 0,
            pixel_height: 0,
        };
        let mut engine = TerminalEngine::new(size);
        engine.feed(b"hello\r\nworld");
        let before = engine.snapshot();
        engine.feed(b"\x1b[1;1H\x1b[JAAA");
        let after = engine.snapshot();
        let app = App::new(AppConfig::from_env());

        let buffer = app
            .build_live_surface_snapshot_delta(Some(&before), Some(&after))
            .expect("snapshot delta should exist");

        assert!(!buffer.contains("\x1b[J"));
        assert!(!buffer.contains("\x1b[2J"));
        assert!(buffer.contains("\x1b[1;6r"));
        assert!(!buffer.contains("\x1b[r"));
        assert!(buffer.contains(&after.styled_lines[0]));
    }

    #[test]
    fn live_surface_snapshot_delta_only_redraws_changed_rows() {
        let size = TerminalSize {
            rows: 4,
            cols: 8,
            pixel_width: 0,
            pixel_height: 0,
        };
        let mut engine = TerminalEngine::new(size);
        engine.feed(b"row1\r\nrow2");
        let before = engine.snapshot();
        engine.feed(b"\x1b[2;1HROW2");
        let after = engine.snapshot();
        let app = App::new(AppConfig::from_env());

        let buffer = app
            .build_live_surface_snapshot_delta(Some(&before), Some(&after))
            .expect("snapshot delta should exist");

        assert!(!buffer.contains(&format!("\x1b[1;1H{}", after.styled_lines[0])));
        assert!(buffer.contains(&format!("\x1b[2;1H{}", after.styled_lines[1])));
    }

    #[test]
    fn native_fullscreen_snapshot_seed_populates_history_before_repainting_screen() {
        let snapshot = crate::terminal::ScreenSnapshot {
            size: TerminalSize {
                rows: 2,
                cols: 5,
                pixel_width: 0,
                pixel_height: 0,
            },
            lines: vec!["two  ".to_string(), "three".to_string()],
            styled_lines: vec!["two  ".to_string(), "three".to_string()],
            active_style_ansi: "\x1b[0m".to_string(),
            scrollback: vec!["one  ".to_string()],
            styled_scrollback: vec!["one  ".to_string()],
            scroll_top: 0,
            scroll_bottom: 1,
            window_title: None,
            cursor_row: 1,
            cursor_col: 5,
            cursor_visible: true,
            alternate_screen: false,
        };
        let app = App::new(AppConfig::from_env());

        let buffer = app
            .build_native_fullscreen_snapshot_seed(&snapshot)
            .expect("replayed fullscreen buffer should build");

        assert!(buffer.starts_with("\x1b[3J\x1b[2J\x1b[H\x1b[?25l"));
        assert!(buffer.contains("one"));
        assert!(buffer.contains("two"));
        assert!(buffer.contains("three"));
        assert!(buffer.contains("\x1b[2;6H"));
    }

    #[test]
    fn live_output_skips_snapshot_redraw_for_bootstrapping_known_live_command() {
        let session = SessionAddress::new("local", "session-1");
        let mut live_surface = LiveSurfaceState::default();
        live_surface.pending_redraw = true;
        live_surface.mark_known_live_command(session.clone());
        live_surface.mark_session_bootstrapping(session.clone(), now_unix_ms());

        assert!(live_output_can_skip_snapshot_redraw(
            &live_surface,
            &session,
            b"codex\r\n",
            now_unix_ms(),
        ));
    }

    #[test]
    fn live_output_keeps_snapshot_redraw_for_non_bootstrapping_shell_echo() {
        let session = SessionAddress::new("local", "session-1");
        let mut live_surface = LiveSurfaceState::default();
        live_surface.pending_redraw = true;

        assert!(!live_output_can_skip_snapshot_redraw(
            &live_surface,
            &session,
            b"plain shell\r\n",
            now_unix_ms(),
        ));
    }

    #[test]
    fn ui_write_payload_wraps_standard_frame_updates_in_sync_markers() {
        assert_eq!(
            build_ui_write_payload("frame", true),
            format!("{ANSI_SYNC_UPDATE_START}frame{ANSI_SYNC_UPDATE_END}")
        );
    }

    #[test]
    fn ui_write_payload_leaves_live_passthrough_chrome_unwrapped() {
        assert_eq!(build_ui_write_payload("frame", false), "frame");
    }

    #[test]
    fn strip_sync_update_markers_removes_nested_agent_batches() {
        let bytes = format!("pre{ANSI_SYNC_UPDATE_START}mid{ANSI_SYNC_UPDATE_END}post");
        assert_eq!(strip_sync_update_markers(bytes.as_bytes()), b"premidpost");
    }

    #[test]
    fn strip_sync_update_markers_stream_handles_chunk_split_markers() {
        let mut pending = Vec::new();
        let first = strip_sync_update_markers_stream(b"pre\x1b[?20", &mut pending);
        let second = strip_sync_update_markers_stream(b"26hmid\x1b[?2026lpost", &mut pending);

        assert_eq!(first, b"pre");
        assert_eq!(second, b"midpost");
        assert!(pending.is_empty());
    }

    #[test]
    fn extract_live_output_batches_emits_plain_output_immediately() {
        let mut pending_tail = Vec::new();
        let mut pending_batch = Vec::new();
        let mut sync_batch_open = false;

        let batches = extract_live_output_batches(
            b"plain output",
            &mut pending_tail,
            &mut pending_batch,
            &mut sync_batch_open,
        );

        assert_eq!(batches, vec![b"plain output".to_vec()]);
        assert!(pending_tail.is_empty());
        assert!(pending_batch.is_empty());
        assert!(!sync_batch_open);
    }

    #[test]
    fn extract_live_output_batches_buffers_until_sync_batch_closes() {
        let mut pending_tail = Vec::new();
        let mut pending_batch = Vec::new();
        let mut sync_batch_open = false;

        let first = extract_live_output_batches(
            b"pre\x1b[?2026hmid",
            &mut pending_tail,
            &mut pending_batch,
            &mut sync_batch_open,
        );
        let second = extract_live_output_batches(
            b"dle\x1b[?2026lpost",
            &mut pending_tail,
            &mut pending_batch,
            &mut sync_batch_open,
        );

        assert_eq!(first, vec![b"pre".to_vec()]);
        assert_eq!(second, vec![b"middle".to_vec(), b"post".to_vec()]);
        assert!(pending_tail.is_empty());
        assert!(pending_batch.is_empty());
        assert!(!sync_batch_open);
    }

    #[test]
    fn extract_live_output_batches_handles_marker_split_across_chunks() {
        let mut pending_tail = Vec::new();
        let mut pending_batch = Vec::new();
        let mut sync_batch_open = false;

        let first = extract_live_output_batches(
            b"\x1b[?20",
            &mut pending_tail,
            &mut pending_batch,
            &mut sync_batch_open,
        );
        let second = extract_live_output_batches(
            b"26hframe\x1b[?202",
            &mut pending_tail,
            &mut pending_batch,
            &mut sync_batch_open,
        );
        let third = extract_live_output_batches(
            b"6l",
            &mut pending_tail,
            &mut pending_batch,
            &mut sync_batch_open,
        );

        assert!(first.is_empty());
        assert!(second.is_empty());
        assert_eq!(third, vec![b"frame".to_vec()]);
        assert!(pending_tail.is_empty());
        assert!(pending_batch.is_empty());
        assert!(!sync_batch_open);
    }

    #[test]
    fn extract_live_output_batches_handles_multiple_sync_batches_in_one_chunk() {
        let mut pending_tail = Vec::new();
        let mut pending_batch = Vec::new();
        let mut sync_batch_open = false;

        let bytes = format!(
            "pre{ANSI_SYNC_UPDATE_START}one{ANSI_SYNC_UPDATE_END}mid{ANSI_SYNC_UPDATE_START}two{ANSI_SYNC_UPDATE_END}post"
        );
        let batches = extract_live_output_batches(
            bytes.as_bytes(),
            &mut pending_tail,
            &mut pending_batch,
            &mut sync_batch_open,
        );

        assert_eq!(
            batches,
            vec![
                b"pre".to_vec(),
                b"one".to_vec(),
                b"mid".to_vec(),
                b"two".to_vec(),
                b"post".to_vec(),
            ]
        );
        assert!(pending_tail.is_empty());
        assert!(pending_batch.is_empty());
        assert!(!sync_batch_open);
    }

    #[test]
    fn takeover_detection_turns_on_for_codex_style_bootstrap_output() {
        assert!(looks_like_terminal_takeover_output(
            b"\x1b[?2026h\x1b[?25l\x1b[1;55H"
        ));
        assert!(looks_like_terminal_takeover_output(b"\x1b[?25l\x1b[1;55H"));
    }

    #[test]
    fn takeover_detection_stays_off_for_codex_incremental_sync_output() {
        assert!(!looks_like_terminal_takeover_output(
            b"\x1b[?2026h\x1b[1;16r\x1b[1S\x1b[r"
        ));
    }

    #[test]
    fn takeover_detection_stays_off_for_probe_only_output() {
        assert!(!looks_like_terminal_takeover_output(
            b"\x1b[?1004h\x1b[6n\x1b]10;?\x1b\\"
        ));
    }

    #[test]
    fn terminal_release_detection_turns_on_for_alt_screen_exit_sequences() {
        assert!(looks_like_terminal_release_output(b"\x1b[?1049l"));
        assert!(looks_like_terminal_release_output(b"\x1b[?1047l"));
        assert!(!looks_like_terminal_release_output(b"\x1b[?1049h"));
    }

    #[test]
    fn substantive_output_detects_visible_prompt_text() {
        assert!(output_is_substantive(b"\x1b[?2004hk@k:/tmp$ "));
        assert!(output_is_substantive("你好".as_bytes()));
    }

    #[test]
    fn shell_prompt_detection_matches_prompt_like_output() {
        assert!(looks_like_shell_prompt_output(b"\x1b[?2004hk@k:/tmp$ "));
        assert!(looks_like_shell_prompt_output(b"user@host % "));
        assert!(!looks_like_shell_prompt_output(
            b"\x1b[?2026h\x1b[1;55H\x1b[0m\x1b[m\x1b[K"
        ));
        assert!(!looks_like_shell_prompt_output(
            b"\r\n>_ OpenAI Codex (v0.120.0)"
        ));
    }

    #[test]
    fn substantive_output_ignores_control_only_heartbeat() {
        assert!(!output_is_substantive(
            b"\x1b[?2026h\x1b[1;55H\x1b[0m\x1b[m\x1b[K\x1b[?25l\x1b[?2026l"
        ));
        assert!(!output_is_substantive(b"\x1b]10;?\x1b\\\x1b[6n"));
    }

    #[test]
    fn next_runtime_event_prioritizes_pending_input_over_output() {
        let (tx, rx) = mpsc::channel();
        tx.send(RuntimeEvent::Output {
            session: SessionAddress::new("local", "session-1"),
            bytes: b"old-output".to_vec(),
        })
        .expect("output should enqueue");
        tx.send(RuntimeEvent::Input(vec![SHORTCUT_PREVIOUS_SESSION]))
            .expect("input should enqueue");

        let mut pending_events = VecDeque::new();
        buffer_pending_runtime_events(&rx, &mut pending_events);

        let event =
            next_runtime_event(&rx, &mut pending_events).expect("pending input should be returned");

        assert!(matches!(
            event,
            RuntimeEvent::Input(bytes) if bytes == vec![SHORTCUT_PREVIOUS_SESSION]
        ));
        assert!(matches!(
            pending_events.front(),
            Some(RuntimeEvent::Output { .. })
        ));
    }

    #[test]
    fn next_runtime_event_returns_pending_output_when_no_input_is_waiting() {
        let (tx, rx) = mpsc::channel();
        tx.send(RuntimeEvent::Output {
            session: SessionAddress::new("local", "session-1"),
            bytes: b"only-output".to_vec(),
        })
        .expect("output should enqueue");

        let mut pending_events = VecDeque::new();
        buffer_pending_runtime_events(&rx, &mut pending_events);

        let event = next_runtime_event(&rx, &mut pending_events)
            .expect("pending output should be returned");

        assert!(matches!(
            event,
            RuntimeEvent::Output { bytes, .. } if bytes == b"only-output".to_vec()
        ));
        assert!(pending_events.is_empty());
    }

    #[test]
    fn derives_shell_title_from_command_line_and_live_command_label() {
        assert_eq!(shell_title_from_command_line("/bin/bash -l"), "bash");
        assert_eq!(
            live_command_label("codex --model gpt-5.4"),
            Some("codex".to_string())
        );
        assert_eq!(
            live_command_label("/tmp/claude-code --dangerously-skip-permissions"),
            Some("claude-code".to_string())
        );
        assert_eq!(live_command_label("bash -lc pwd"), None);
    }

    #[test]
    fn foreground_title_decision_prefers_foreground_live_agent_and_restores_shell() {
        assert_eq!(
            foreground_title_decision("/bin/bash -l", Some("codex"), false),
            ForegroundTitleDecision::SetLive("codex".to_string())
        );
        assert_eq!(
            foreground_title_decision("/bin/bash -l", Some("bash"), false),
            ForegroundTitleDecision::RestoreShell("bash".to_string())
        );
        assert_eq!(
            foreground_title_decision("/bin/bash -l", Some("bash"), true),
            ForegroundTitleDecision::Keep
        );
        assert_eq!(
            foreground_title_decision("/bin/bash -l", Some("python3"), false),
            ForegroundTitleDecision::Keep
        );
    }

    #[test]
    fn foreground_process_inspection_errors_are_nonfatal() {
        assert!(nonfatal_foreground_process_inspection_error(
            &PtyError::Inspect(io::Error::new(io::ErrorKind::NotFound, "missing process"),)
        ));
        assert!(!nonfatal_foreground_process_inspection_error(
            &PtyError::Read(io::Error::new(io::ErrorKind::BrokenPipe, "pty read failed"),)
        ));
    }

    #[test]
    fn shell_command_tracker_submits_on_plain_enter() {
        let mut tracker = ShellCommandTracker::default();

        assert_eq!(tracker.observe(b"codex\r"), Some("codex".to_string()));
    }

    #[test]
    fn shell_command_tracker_submits_on_kitty_enter_without_losing_buffer() {
        let mut tracker = ShellCommandTracker::default();

        assert_eq!(tracker.observe(b"codex\x1b[13u"), Some("codex".to_string()));
    }

    #[test]
    fn shell_command_tracker_submits_on_split_kitty_enter_sequence() {
        let mut tracker = ShellCommandTracker::default();

        assert_eq!(tracker.observe(b"codex\x1b["), None);
        assert_eq!(tracker.observe(b"13u"), Some("codex".to_string()));
    }

    #[test]
    fn shell_command_tracker_submits_on_application_keypad_enter() {
        let mut tracker = ShellCommandTracker::default();

        assert_eq!(tracker.observe(b"codex\x1bOM"), Some("codex".to_string()));
    }

    #[test]
    fn shell_command_tracker_submits_on_modify_other_keys_enter() {
        let mut tracker = ShellCommandTracker::default();

        assert_eq!(
            tracker.observe(b"codex\x1b[27;13;13~"),
            Some("codex".to_string())
        );
    }

    #[test]
    fn shell_command_tracker_exposes_pending_live_command_label() {
        let mut tracker = ShellCommandTracker::default();

        assert_eq!(tracker.observe(b"cod"), None);
        assert_eq!(tracker.pending_live_command_label(), None);

        assert_eq!(tracker.observe(b"ex"), None);
        assert_eq!(
            tracker.pending_live_command_label(),
            Some("codex".to_string())
        );

        assert_eq!(tracker.observe(b"x"), None);
        assert_eq!(tracker.pending_live_command_label(), None);
    }

    #[test]
    fn submit_detection_matches_plain_and_escape_enter_sequences() {
        assert!(bytes_include_submit(b"\r"));
        assert!(bytes_include_submit(b"\x1bOM"));
        assert!(bytes_include_submit(b"\x1b[13u"));
        assert!(!bytes_include_submit(b"\x1b[A"));
    }

    #[test]
    fn extracts_command_from_shell_prompt_line() {
        assert_eq!(
            extract_shell_prompt_command("k@k:/opt/data/workspace/wait-agent$ codex"),
            Some("codex".to_string())
        );
        assert_eq!(
            extract_shell_prompt_command("user@host % claude --continue"),
            Some("claude --continue".to_string())
        );
        assert_eq!(extract_shell_prompt_command("prompt$ "), None);
    }

    #[test]
    fn snapshot_command_inference_detects_live_agent_recalled_from_history() {
        let mut engine = TerminalEngine::new(TerminalSize {
            rows: 2,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        });
        engine.feed(b"k@k:/opt/data/workspace/wait-agent$ codex");

        assert_eq!(
            live_command_label_from_shell_snapshot(engine.state().active_snapshot()),
            Some("codex".to_string())
        );
    }

    #[test]
    fn native_fullscreen_seed_replays_normal_screen_history_at_target_width() {
        let mut engine = TerminalEngine::new(TerminalSize {
            rows: 2,
            cols: 8,
            pixel_width: 0,
            pixel_height: 0,
        });
        let mut transcript = TerminalTranscript::default();
        transcript.record_output(b"1234567890");
        engine.feed(b"1234567890");

        let seed = native_fullscreen_seed_snapshot(
            &transcript,
            TerminalSize {
                rows: 2,
                cols: 12,
                pixel_width: 0,
                pixel_height: 0,
            },
        );

        assert_eq!(engine.state().active_snapshot().lines[0], "12345678");
        assert_eq!(seed.lines[0], "1234567890  ");
        assert_eq!(seed.lines[1], "            ");
        assert!(!seed.alternate_screen);
    }

    #[test]
    fn native_fullscreen_seed_replays_alternate_screen_history_at_target_width() {
        let mut transcript = TerminalTranscript::default();
        transcript.record_output(b"prompt$ \x1b[?1049hone\r\ntwo\r\nthree");

        let seed = native_fullscreen_seed_snapshot(
            &transcript,
            TerminalSize {
                rows: 2,
                cols: 12,
                pixel_width: 0,
                pixel_height: 0,
            },
        );

        assert!(seed.alternate_screen);
        assert_eq!(seed.scrollback, vec!["one         ".to_string()]);
        assert_eq!(seed.lines[0], "two         ");
        assert_eq!(seed.lines[1], "three       ");
    }

    #[test]
    fn native_fullscreen_seed_replays_normal_screen_app_history_without_shell_heuristics() {
        let narrow_size = TerminalSize {
            rows: 3,
            cols: 8,
            pixel_width: 0,
            pixel_height: 0,
        };
        let wide_size = TerminalSize {
            rows: 3,
            cols: 16,
            pixel_width: 0,
            pixel_height: 0,
        };

        let app_output =
            b"codex resume 019d9f0f-fd40-77e2-bb15-11b4c81129b7\r\nprogress: replay me";

        let mut transcript = TerminalTranscript::default();
        transcript.record_output(app_output);

        let mut engine = TerminalEngine::new(narrow_size);
        engine.feed(app_output);
        let narrow_snapshot = engine.state().active_snapshot().clone();

        let seed = native_fullscreen_seed_snapshot(&transcript, wide_size);

        assert!(!narrow_snapshot.alternate_screen);
        assert_eq!(narrow_snapshot.scrollback[0], "codex re");
        assert_eq!(narrow_snapshot.lines[0], "progress");
        assert_eq!(seed.scrollback[0], "codex resume 019");
        assert_eq!(seed.scrollback[1], "d9f0f-fd40-77e2-");
        assert_ne!(seed.lines, narrow_snapshot.lines);
        assert_ne!(seed.scrollback, narrow_snapshot.scrollback);
    }
}
