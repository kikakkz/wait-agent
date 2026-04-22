use crate::application::session_service::SessionService;
use crate::cli::UiPaneCommand;
use crate::infra::tmux::{EmbeddedTmuxBackend, TmuxError};
use crate::lifecycle::LifecycleError;
use crate::ui::footer::FooterUi;
use crate::ui::sidebar::SidebarUi;
use std::io::{self, Write};
use std::thread;
use std::time::Duration;

const PANE_REFRESH_INTERVAL: Duration = Duration::from_millis(750);

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
        loop {
            let sessions = self
                .session_service
                .list_sessions()
                .map_err(ui_pane_error)?;
            let (width, height) = self.pane_size(&command.socket_name);
            redraw(SidebarUi::render(
                &command.socket_name,
                &command.session_name,
                &sessions,
                width,
                height,
            ))?;
            thread::sleep(PANE_REFRESH_INTERVAL);
        }
    }

    pub fn run_footer(&self, command: UiPaneCommand) -> Result<(), LifecycleError> {
        loop {
            let sessions = self
                .session_service
                .list_sessions()
                .map_err(ui_pane_error)?;
            let (width, _) = self.pane_size(&command.socket_name);
            redraw(FooterUi::render(
                &command.socket_name,
                &command.session_name,
                &sessions,
                width,
            ))?;
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
}

fn redraw(buffer: String) -> Result<(), LifecycleError> {
    let mut stdout = io::stdout().lock();
    write!(stdout, "\x1b[2J\x1b[H{buffer}\x1b[0m").map_err(|error| {
        LifecycleError::Io("failed to draw waitagent pane UI".to_string(), error)
    })?;
    stdout
        .flush()
        .map_err(|error| LifecycleError::Io("failed to flush waitagent pane UI".to_string(), error))
}

fn ui_pane_error(error: TmuxError) -> LifecycleError {
    LifecycleError::Io(
        "failed to render waitagent pane UI".to_string(),
        io::Error::new(io::ErrorKind::Other, error.to_string()),
    )
}
