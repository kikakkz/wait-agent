use crate::application::target_registry_service::{
    DefaultTargetCatalogGateway, TargetRegistryService,
};
use crate::cli::UiPaneCommand;
use crate::domain::workspace::WorkspaceInstanceId;
use crate::infra::tmux::{
    EmbeddedTmuxBackend, TmuxLayoutGateway, TmuxSessionName, TmuxSocketName, TmuxWorkspaceHandle,
};
use crate::lifecycle::LifecycleError;
use crate::runtime::event_driven_chrome_runtime::EventDrivenChromeRenderUpdate;
use crate::runtime::event_driven_tmux_pane_runtime::EventDrivenTmuxPaneRuntime;
use crate::runtime::event_driven_ui_pane_runtime::EventDrivenSidebarActivation;
use crate::terminal::TerminalRuntime;
use std::io::{self, Read, Write};
use std::os::raw::{c_int, c_void};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread;

const SIGWINCH: c_int = 28;

static PANE_SIGWINCH_WRITE_FD: AtomicI32 = AtomicI32::new(-1);

extern "C" {
    fn signal(signum: c_int, handler: extern "C" fn(c_int)) -> usize;
    fn write(fd: c_int, buf: *const c_void, count: usize) -> isize;
}

const FULLSCREEN_FOOTER_OPTION: &str = "@waitagent_fullscreen_footer_line";
const HIDE_CURSOR_ESCAPE: &str = "\x1b[?25l";
const SHOW_CURSOR_ESCAPE: &str = "\x1b[?25h";

pub struct EventDrivenPaneRuntime {
    backend: EmbeddedTmuxBackend,
}

impl EventDrivenPaneRuntime {
    pub fn from_build_env() -> Result<Self, LifecycleError> {
        let backend = EmbeddedTmuxBackend::from_build_env().map_err(event_pane_error)?;
        Ok(Self { backend })
    }

    pub fn run_sidebar(&self, command: UiPaneCommand) -> Result<(), LifecycleError> {
        let pane_target = current_tmux_pane_target();
        let terminal = TerminalRuntime::stdio();
        let _raw_mode = terminal.enter_raw_mode()?;
        let _cursor_guard = PaneCursorGuard::hide().map_err(|error| {
            LifecycleError::Io("failed to hide sidebar cursor".to_string(), error)
        })?;
        let event_rx =
            spawn_pane_event_stream(self.backend.clone(), &command, true).map_err(|error| {
                LifecycleError::Io("failed to install pane event watchers".to_string(), error)
            })?;
        let mut chrome = EventDrivenTmuxPaneRuntime::new_with_target_registry(
            self.backend.clone(),
            TargetRegistryService::new(
                DefaultTargetCatalogGateway::from_build_env().map_err(event_pane_error)?,
            ),
        );
        let mut last_buffer = String::new();

        redraw_sidebar(
            chrome.refresh_sidebar_for_pane(&command, pane_target.as_deref().unwrap_or(""))?,
            &mut last_buffer,
        )?;
        self.mark_initial_sidebar_ready(&command, pane_target.as_deref())?;

        loop {
            let event = match event_rx.recv() {
                Ok(event) => event,
                Err(_) => return Ok(()),
            };
            match event {
                PaneEvent::Input(bytes) => {
                    let outcome = chrome.apply_sidebar_input(&bytes);
                    redraw_sidebar(outcome.render, &mut last_buffer)?;
                    if let Some(activation) = outcome.activation {
                        self.apply_sidebar_activation(&command, activation)?;
                    }
                }
                PaneEvent::Resize | PaneEvent::Refresh => redraw_sidebar(
                    chrome
                        .refresh_sidebar_for_pane(&command, pane_target.as_deref().unwrap_or(""))?,
                    &mut last_buffer,
                )?,
            }
        }
    }

    pub fn run_footer(&self, command: UiPaneCommand) -> Result<(), LifecycleError> {
        let pane_target = current_tmux_pane_target();
        let event_rx =
            spawn_pane_event_stream(self.backend.clone(), &command, false).map_err(|error| {
                LifecycleError::Io("failed to install pane event watchers".to_string(), error)
            })?;
        let mut chrome = EventDrivenTmuxPaneRuntime::new_with_target_registry(
            self.backend.clone(),
            TargetRegistryService::new(
                DefaultTargetCatalogGateway::from_build_env().map_err(event_pane_error)?,
            ),
        );
        let mut last_buffer = String::new();
        let workspace = workspace_handle(&command);
        let update =
            chrome.refresh_footer_for_pane(&command, pane_target.as_deref().unwrap_or(""))?;
        apply_footer_update(&self.backend, &workspace, update, &mut last_buffer)?;
        self.mark_initial_footer_ready(&command, pane_target.as_deref())?;

        loop {
            let event = match event_rx.recv() {
                Ok(event) => event,
                Err(_) => return Ok(()),
            };
            if matches!(event, PaneEvent::Resize | PaneEvent::Refresh) {
                let update = chrome
                    .refresh_footer_for_pane(&command, pane_target.as_deref().unwrap_or(""))?;
                apply_footer_update(&self.backend, &workspace, update, &mut last_buffer)?;
            }
        }
    }

    fn apply_sidebar_activation(
        &self,
        command: &UiPaneCommand,
        activation: EventDrivenSidebarActivation,
    ) -> Result<(), LifecycleError> {
        match activation {
            EventDrivenSidebarActivation::SelectMainPane => self
                .backend
                .run_socket_command(
                    &TmuxSocketName::new(&command.socket_name),
                    &["select-pane".to_string(), "-L".to_string()],
                )
                .map_err(event_pane_error),
            EventDrivenSidebarActivation::ActivateTarget { target } => self
                .backend
                .run_socket_command(
                    &TmuxSocketName::new(&command.socket_name),
                    &activate_target_run_shell_args(
                        &current_executable_string()?,
                        &command.socket_name,
                        &command.session_name,
                        &target,
                    ),
                )
                .map_err(event_pane_error),
        }
    }

    fn mark_initial_sidebar_ready(
        &self,
        command: &UiPaneCommand,
        pane_target: Option<&str>,
    ) -> Result<(), LifecycleError> {
        let Some(pane_target) = pane_target else {
            return Ok(());
        };
        self.backend
            .mark_sidebar_ready(&workspace_handle(command), pane_target)
            .map_err(event_pane_error)
    }

    fn mark_initial_footer_ready(
        &self,
        command: &UiPaneCommand,
        pane_target: Option<&str>,
    ) -> Result<(), LifecycleError> {
        let Some(pane_target) = pane_target else {
            return Ok(());
        };
        self.backend
            .mark_footer_ready(&workspace_handle(command), pane_target)
            .map_err(event_pane_error)
    }
}

fn redraw_sidebar(
    update: EventDrivenChromeRenderUpdate,
    last_buffer: &mut String,
) -> Result<(), LifecycleError> {
    if update.invalidate_sidebar {
        last_buffer.clear();
    }
    if let Some(buffer) = update.sidebar {
        redraw_if_changed(buffer, last_buffer)?;
    }
    Ok(())
}

fn apply_footer_update(
    backend: &EmbeddedTmuxBackend,
    workspace: &TmuxWorkspaceHandle,
    update: EventDrivenChromeRenderUpdate,
    last_buffer: &mut String,
) -> Result<(), LifecycleError> {
    if update.invalidate_footer {
        last_buffer.clear();
    }
    if let Some(buffer) = update.footer {
        redraw_if_changed(buffer, last_buffer)?;
    }
    if let Some(status) = update.fullscreen_status {
        backend
            .set_session_option(workspace, FULLSCREEN_FOOTER_OPTION, &status)
            .map_err(event_pane_error)?;
    }
    Ok(())
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

    write!(stdout, "\x1b[0m")?;
    Ok(())
}

#[derive(Debug)]
enum PaneEvent {
    Input(Vec<u8>),
    Resize,
    Refresh,
}

struct PaneResizeWatcher {
    _writer: UnixStream,
}

struct PaneCursorGuard {
    visible_on_drop: bool,
}

impl PaneCursorGuard {
    fn hide() -> io::Result<Self> {
        write_escape(HIDE_CURSOR_ESCAPE)?;
        Ok(Self {
            visible_on_drop: true,
        })
    }
}

impl Drop for PaneCursorGuard {
    fn drop(&mut self) {
        if self.visible_on_drop {
            let _ = write_escape(SHOW_CURSOR_ESCAPE);
        }
    }
}

impl Drop for PaneResizeWatcher {
    fn drop(&mut self) {
        PANE_SIGWINCH_WRITE_FD.store(-1, Ordering::Relaxed);
    }
}

fn spawn_pane_event_stream(
    backend: EmbeddedTmuxBackend,
    command: &UiPaneCommand,
    include_input: bool,
) -> io::Result<Receiver<PaneEvent>> {
    let (tx, rx) = mpsc::channel();
    if include_input {
        spawn_input_thread(tx.clone());
    }
    let _resize_watcher = spawn_resize_watcher(tx.clone())?;
    spawn_chrome_refresh_watcher(
        backend,
        command.socket_name.clone(),
        command.session_name.clone(),
        tx.clone(),
    );
    thread::spawn(move || {
        let _keep_resize_watcher_alive = _resize_watcher;
        thread::park();
    });
    Ok(rx)
}

fn spawn_input_thread(tx: mpsc::Sender<PaneEvent>) {
    thread::spawn(move || {
        let mut stdin = io::stdin().lock();
        let mut buffer = [0u8; 64];
        loop {
            match stdin.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    if tx.send(PaneEvent::Input(buffer[..read].to_vec())).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
}

fn write_escape(sequence: &str) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    stdout.write_all(sequence.as_bytes())?;
    stdout.flush()
}

fn spawn_chrome_refresh_watcher(
    backend: EmbeddedTmuxBackend,
    socket_name: String,
    session_name: String,
    tx: mpsc::Sender<PaneEvent>,
) {
    thread::spawn(move || loop {
        if backend
            .wait_for_chrome_refresh_on_socket(&socket_name, &session_name)
            .is_err()
        {
            break;
        }
        if tx.send(PaneEvent::Refresh).is_err() {
            break;
        }
    });
}

fn current_tmux_pane_target() -> Option<String> {
    std::env::var("TMUX_PANE").ok()
}

fn activate_target_shell_command(
    executable: &str,
    current_socket_name: &str,
    current_session_name: &str,
    target: &str,
) -> String {
    [
        shell_escape(executable),
        shell_escape("__activate-target"),
        shell_escape("--current-socket-name"),
        shell_escape(current_socket_name),
        shell_escape("--current-session-name"),
        shell_escape(current_session_name),
        shell_escape("--target"),
        shell_escape(target),
    ]
    .join(" ")
}

fn activate_target_run_shell_args(
    executable: &str,
    current_socket_name: &str,
    current_session_name: &str,
    target: &str,
) -> Vec<String> {
    vec![
        "run-shell".to_string(),
        "-b".to_string(),
        activate_target_shell_command(
            executable,
            current_socket_name,
            current_session_name,
            target,
        ),
    ]
}

fn spawn_resize_watcher(tx: mpsc::Sender<PaneEvent>) -> io::Result<PaneResizeWatcher> {
    let (mut reader, writer) = UnixStream::pair()?;
    PANE_SIGWINCH_WRITE_FD.store(writer.as_raw_fd(), Ordering::Relaxed);
    unsafe {
        signal(SIGWINCH, pane_sigwinch_handler);
    }

    thread::spawn(move || {
        let mut buffer = [0_u8; 64];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(_) => {
                    if tx.send(PaneEvent::Resize).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    Ok(PaneResizeWatcher { _writer: writer })
}

fn workspace_handle(command: &UiPaneCommand) -> TmuxWorkspaceHandle {
    TmuxWorkspaceHandle {
        workspace_id: WorkspaceInstanceId::new(command.session_name.clone()),
        socket_name: TmuxSocketName::new(command.socket_name.clone()),
        session_name: TmuxSessionName::new(command.session_name.clone()),
    }
}

fn current_executable_string() -> Result<String, LifecycleError> {
    std::env::current_exe()
        .map(|path| path.display().to_string())
        .map_err(|error| {
            LifecycleError::Io("failed to locate waitagent executable".to_string(), error)
        })
}

fn shell_escape(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn event_pane_error<E>(error: E) -> LifecycleError
where
    E: ToString,
{
    LifecycleError::Io(
        "failed to run event-driven waitagent pane".to_string(),
        io::Error::new(io::ErrorKind::Other, error.to_string()),
    )
}

extern "C" fn pane_sigwinch_handler(_signal: c_int) {
    let fd = PANE_SIGWINCH_WRITE_FD.load(Ordering::Relaxed);
    if fd < 0 {
        return;
    }

    let byte = 1_u8;
    unsafe {
        let _ = write(fd, (&byte as *const u8).cast::<c_void>(), 1);
    }
}

#[cfg(test)]
mod tests {
    use super::{activate_target_run_shell_args, write_buffer};

    #[test]
    fn single_line_pane_render_does_not_issue_clear_below_buffer() {
        let mut output = Vec::new();

        write_buffer(&mut output, "keys: ^W cmd").expect("footer render should write");

        let rendered = String::from_utf8(output).expect("writer should emit utf8 escape payload");
        assert!(rendered.contains("\x1b[1;1Hkeys: ^W cmd\x1b[K"));
        assert!(!rendered.contains("\x1b[2;1H\x1b[J"));
    }

    #[test]
    fn multi_line_pane_render_keeps_row_local_clears_only() {
        let mut output = Vec::new();

        write_buffer(&mut output, "line1\nline2").expect("sidebar render should write");

        let rendered = String::from_utf8(output).expect("writer should emit utf8 escape payload");
        assert!(rendered.contains("\x1b[1;1Hline1\x1b[K"));
        assert!(rendered.contains("\x1b[2;1Hline2\x1b[K"));
        assert!(!rendered.contains("\x1b[J"));
    }

    #[test]
    fn activate_target_run_shell_args_pass_shell_command_without_tmux_layer_requoting() {
        let args = activate_target_run_shell_args(
            "/tmp/wait agent",
            "wa-1",
            "session-1",
            "wa-1:session-2",
        );

        assert_eq!(
            args,
            vec![
                "run-shell".to_string(),
                "-b".to_string(),
                "'/tmp/wait agent' '__activate-target' '--current-socket-name' 'wa-1' '--current-session-name' 'session-1' '--target' 'wa-1:session-2'"
                    .to_string(),
            ]
        );
    }
}
