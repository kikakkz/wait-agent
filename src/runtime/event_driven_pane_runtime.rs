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
use std::sync::mpsc::{self, Receiver};

const FULLSCREEN_FOOTER_OPTION: &str = "@waitagent_fullscreen_footer_line";

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
        let input = spawn_input_thread();
        let mut chrome = EventDrivenTmuxPaneRuntime::new(self.backend.clone());
        let mut last_buffer = String::new();

        redraw_sidebar(
            chrome.refresh_sidebar_for_pane(&command, pane_target.as_deref().unwrap_or(""))?,
            &mut last_buffer,
        )?;

        loop {
            let bytes = match input.recv() {
                Ok(bytes) => bytes,
                Err(_) => return Ok(()),
            };
            let outcome = chrome.apply_sidebar_input(&bytes);
            redraw_sidebar(outcome.render, &mut last_buffer)?;
            if let Some(activation) = outcome.activation {
                self.apply_sidebar_activation(&command, activation)?;
            }
        }
    }

    pub fn run_footer(&self, command: UiPaneCommand) -> Result<(), LifecycleError> {
        let pane_target = current_tmux_pane_target();
        let mut chrome = EventDrivenTmuxPaneRuntime::new(self.backend.clone());
        let mut last_buffer = String::new();
        let workspace = workspace_handle(&command);
        let update =
            chrome.refresh_footer_for_pane(&command, pane_target.as_deref().unwrap_or(""))?;
        apply_footer_update(&self.backend, &workspace, update, &mut last_buffer)?;

        wait_until_killed();
        Ok(())
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
            EventDrivenSidebarActivation::AttachSession { target } => self
                .backend
                .run_socket_command(
                    &TmuxSocketName::new(&command.socket_name),
                    &[
                        "detach-client".to_string(),
                        "-E".to_string(),
                        format!(
                            "{} attach {}",
                            shell_escape(&current_executable_string()?),
                            shell_escape(&target)
                        ),
                    ],
                )
                .map_err(event_pane_error),
        }
    }
}

fn redraw_sidebar(
    update: EventDrivenChromeRenderUpdate,
    last_buffer: &mut String,
) -> Result<(), LifecycleError> {
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

fn spawn_input_thread() -> Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
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

fn current_tmux_pane_target() -> Option<String> {
    std::env::var("TMUX_PANE").ok()
}

fn wait_until_killed() {
    loop {
        std::thread::park();
    }
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

#[cfg(test)]
mod tests {
    use super::write_buffer;

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
}
