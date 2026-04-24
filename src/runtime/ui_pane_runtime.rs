// Historical polling implementation kept on disk for migration reference only.
// The accepted default pane path now runs through `event_driven_pane_runtime`.

use crate::application::session_service::SessionService;
use crate::cli::UiPaneCommand;
use crate::domain::session_catalog::ManagedSessionRecord;
use crate::domain::workspace::WorkspaceInstanceId;
use crate::infra::tmux::{
    EmbeddedTmuxBackend, TmuxError, TmuxLayoutGateway, TmuxSessionName, TmuxSocketName,
    TmuxWorkspaceHandle,
};
use crate::lifecycle::LifecycleError;
use crate::terminal::TerminalRuntime;
use crate::ui::footer::FooterUi;
use crate::ui::sidebar::SidebarUi;
use std::io::{self, Read, Write};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

const PANE_REFRESH_INTERVAL: Duration = Duration::from_millis(200);
const FULLSCREEN_FOOTER_OPTION: &str = "@waitagent_fullscreen_footer_line";

pub struct UiPaneRuntime {
    backend: EmbeddedTmuxBackend,
    session_service: SessionService<EmbeddedTmuxBackend>,
}

impl UiPaneRuntime {
    pub fn from_build_env() -> Result<Self, LifecycleError> {
        let backend = EmbeddedTmuxBackend::from_build_env().map_err(ui_pane_error)?;
        Ok(Self {
            session_service: SessionService::new(backend.clone()),
            backend,
        })
    }

    pub fn run_sidebar(&self, command: UiPaneCommand) -> Result<(), LifecycleError> {
        let terminal = TerminalRuntime::stdio();
        let _raw_mode = terminal.enter_raw_mode()?;
        let input = spawn_sidebar_input_thread();
        let mut last_buffer = String::new();
        let mut pending_input = Vec::new();
        let mut selected_target = None::<String>;
        loop {
            let sessions = self
                .session_service
                .list_sessions()
                .map_err(ui_pane_error)?;
            sync_selected_target(
                &mut selected_target,
                &command.socket_name,
                &command.session_name,
                &sessions,
            );
            let (width, height) = self.pane_size(&command.socket_name);
            redraw_if_changed(
                SidebarUi::render(
                    &command.socket_name,
                    &command.session_name,
                    selected_target.as_deref(),
                    &sessions,
                    width,
                    height,
                    now_millis(),
                ),
                &mut last_buffer,
            )?;
            match input.recv_timeout(PANE_REFRESH_INTERVAL) {
                Ok(bytes) => {
                    for action in sidebar_actions(&mut pending_input, &bytes) {
                        match action {
                            SidebarInputAction::Previous => {
                                move_selection(&mut selected_target, &sessions, -1);
                            }
                            SidebarInputAction::Next => {
                                move_selection(&mut selected_target, &sessions, 1);
                            }
                            SidebarInputAction::Submit => self
                                .activate_sidebar_selection(
                                    &command.socket_name,
                                    &command.session_name,
                                    selected_target.as_deref(),
                                )
                                .map_err(ui_pane_error)?,
                        }
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return Ok(()),
            }
        }
    }

    pub fn run_footer(&self, command: UiPaneCommand) -> Result<(), LifecycleError> {
        let workspace = workspace_handle(&command);
        let mut last_buffer = String::new();
        let mut last_fullscreen_footer = String::new();
        loop {
            let sessions = self
                .session_service
                .list_sessions()
                .map_err(ui_pane_error)?;
            let (width, _) = self.pane_size(&command.socket_name);
            let footer_line = FooterUi::render(
                &command.socket_name,
                &command.session_name,
                &sessions,
                width,
            );
            let fullscreen_footer_line = FooterUi::render_fullscreen(
                &command.socket_name,
                &command.session_name,
                &sessions,
                width,
            );
            if fullscreen_footer_line != last_fullscreen_footer {
                self.backend
                    .set_session_option(
                        &workspace,
                        FULLSCREEN_FOOTER_OPTION,
                        &fullscreen_footer_line,
                    )
                    .map_err(ui_pane_error)?;
                last_fullscreen_footer = fullscreen_footer_line;
            }
            redraw_if_changed(footer_line, &mut last_buffer)?;
            thread::sleep(PANE_REFRESH_INTERVAL);
        }
    }

    fn pane_size(&self, socket_name: &str) -> (usize, usize) {
        let Some(pane_target) = std::env::var("TMUX_PANE").ok() else {
            return (80, 12);
        };
        self.backend
            .pane_dimensions_on_socket(socket_name, &pane_target)
            .unwrap_or((80, 12))
    }

    fn activate_sidebar_selection(
        &self,
        socket_name: &str,
        session_name: &str,
        selected_target: Option<&str>,
    ) -> Result<(), TmuxError> {
        let Some(selected_target) = selected_target else {
            return Ok(());
        };

        if selected_target == format!("{socket_name}:{session_name}") {
            return self.run_select_main_command(socket_name);
        }

        self.backend.run_socket_command(
            &TmuxSocketName::new(socket_name),
            &[
                "detach-client".to_string(),
                "-E".to_string(),
                format!(
                    "{} attach {}",
                    shell_escape(&current_executable_string()?),
                    shell_escape(selected_target)
                ),
            ],
        )?;
        Ok(())
    }

    fn run_select_main_command(&self, socket_name: &str) -> Result<(), TmuxError> {
        self.backend.run_socket_command(
            &TmuxSocketName::new(socket_name),
            &["select-pane".to_string(), "-L".to_string()],
        )?;
        Ok(())
    }
}

fn redraw_if_changed(buffer: String, last_buffer: &mut String) -> Result<(), LifecycleError> {
    if *last_buffer == buffer {
        return Ok(());
    }

    let mut stdout = io::stdout().lock();
    write_buffer(&mut stdout, &buffer).map_err(|error| {
        LifecycleError::Io("failed to draw waitagent pane UI".to_string(), error)
    })?;
    stdout.flush().map_err(|error| {
        LifecycleError::Io("failed to flush waitagent pane UI".to_string(), error)
    })?;
    *last_buffer = buffer;
    Ok(())
}

fn write_buffer(stdout: &mut impl Write, buffer: &str) -> io::Result<()> {
    for (index, line) in buffer.split('\n').enumerate() {
        let row = index + 1;
        write!(stdout, "\x1b[{row};1H{line}\x1b[K")?;
    }

    let clear_from_row = buffer.split('\n').count() + 1;
    write!(stdout, "\x1b[{clear_from_row};1H\x1b[J\x1b[0m")?;
    Ok(())
}

fn spawn_sidebar_input_thread() -> Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut stdin = io::stdin().lock();
        let mut buffer = [0u8; 64];
        loop {
            match stdin.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    if tx.send(buffer[..read].to_vec()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
    rx
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SidebarInputAction {
    Previous,
    Next,
    Submit,
}

fn sidebar_actions(pending: &mut Vec<u8>, bytes: &[u8]) -> Vec<SidebarInputAction> {
    pending.extend_from_slice(bytes);
    let mut actions = Vec::new();

    loop {
        if pending.starts_with(b"\x1b[A") || pending.starts_with(b"\x1bOA") {
            pending.drain(..3);
            actions.push(SidebarInputAction::Previous);
        } else if pending.starts_with(b"\x1b[B") || pending.starts_with(b"\x1bOB") {
            pending.drain(..3);
            actions.push(SidebarInputAction::Next);
        } else if pending.starts_with(b"\x1bOM") {
            pending.drain(..3);
            actions.push(SidebarInputAction::Submit);
        } else if pending.starts_with(b"\x1b[13u") {
            pending.drain(..5);
            actions.push(SidebarInputAction::Submit);
        } else if pending.first() == Some(&b'\r') || pending.first() == Some(&b'\n') {
            pending.drain(..1);
            actions.push(SidebarInputAction::Submit);
        } else if is_partial_sidebar_sequence(pending) {
            break;
        } else if pending.is_empty() {
            break;
        } else {
            pending.drain(..1);
        }
    }

    actions
}

fn is_partial_sidebar_sequence(pending: &[u8]) -> bool {
    [
        b"\x1b[".as_slice(),
        b"\x1bO".as_slice(),
        b"\x1b[1".as_slice(),
        b"\x1b[13".as_slice(),
    ]
    .iter()
    .any(|pattern| pattern.starts_with(pending))
}

fn sync_selected_target(
    selected_target: &mut Option<String>,
    active_socket: &str,
    active_session: &str,
    sessions: &[ManagedSessionRecord],
) {
    if sessions.is_empty() {
        *selected_target = None;
        return;
    }

    let selected_exists = selected_target.as_ref().map(|target| {
        sessions
            .iter()
            .any(|session| session.address.qualified_target() == *target)
    });
    if selected_exists == Some(true) {
        return;
    }

    *selected_target = sessions
        .iter()
        .find(|session| {
            session.address.server_id() == active_socket
                && session.address.session_id() == active_session
        })
        .or_else(|| sessions.first())
        .map(|session| session.address.qualified_target());
}

fn move_selection(
    selected_target: &mut Option<String>,
    sessions: &[ManagedSessionRecord],
    delta: isize,
) {
    if sessions.is_empty() {
        *selected_target = None;
        return;
    }

    let current_index = selected_target
        .as_ref()
        .and_then(|target| {
            sessions
                .iter()
                .position(|session| session.address.qualified_target() == *target)
        })
        .unwrap_or(0);
    let next_index =
        ((current_index as isize + delta).rem_euclid(sessions.len() as isize)) as usize;
    *selected_target = Some(sessions[next_index].address.qualified_target());
}

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_millis())
        .unwrap_or_default()
}

fn workspace_handle(command: &UiPaneCommand) -> TmuxWorkspaceHandle {
    TmuxWorkspaceHandle {
        workspace_id: WorkspaceInstanceId::new(command.session_name.clone()),
        socket_name: TmuxSocketName::new(command.socket_name.clone()),
        session_name: TmuxSessionName::new(command.session_name.clone()),
    }
}

fn ui_pane_error(error: TmuxError) -> LifecycleError {
    LifecycleError::Io(
        "failed to render waitagent pane UI".to_string(),
        io::Error::new(io::ErrorKind::Other, error.to_string()),
    )
}

fn current_executable_string() -> Result<String, TmuxError> {
    std::env::current_exe()
        .map(|path| path.display().to_string())
        .map_err(|error| TmuxError::new(format!("failed to locate waitagent executable: {error}")))
}

fn shell_escape(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}
