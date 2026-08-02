use crate::cli::{Command, RatatuiClientCommand, RatatuiNodeServerCommand, RemoteNetworkConfig};
use crate::error::AppError;
use crate::runtime::ratatui_client_runtime::RatatuiClientRuntime;
use crate::runtime::ratatui_node_runtime::RatatuiNodeRuntime;
use crate::runtime::ratatui_workspace_runtime::RatatuiWorkspaceRuntime;
use crate::ui::banner::print_banner;

// This dispatcher is the single command-side ownership boundary for the
// accepted local default route. `workspace` and `attach` must continue to flow
// into `WorkspaceCommandRuntime`, while hidden chrome panes stay on the
// dedicated event-driven pane runtimes.
pub struct CommandDispatcher {
    network: RemoteNetworkConfig,
}

impl CommandDispatcher {
    pub fn from_build_env_with_network_and_command(
        network: RemoteNetworkConfig,
        _command: &Command,
    ) -> Result<Self, AppError> {
        Ok(Self { network })
    }

    pub fn dispatch(&self, command: Command) -> Result<(), AppError> {
        match command {
            Command::Workspace => self
                .ratatui_workspace()?
                .run_workspace_entry()
                .map_err(AppError::from),
            Command::Attach(command) => self
                .ratatui_workspace()?
                .attach(command.target)
                .map_err(AppError::from),
            Command::List => self.ratatui_workspace()?.list().map_err(AppError::from),
            Command::Cleanup => self.ratatui_workspace()?.cleanup().map_err(AppError::from),
            Command::Detach(command) => self
                .ratatui_workspace()?
                .detach(command.target)
                .map_err(AppError::from),
            Command::Stop(command) => self
                .ratatui_workspace()?
                .stop(command.target)
                .map_err(AppError::from),
            Command::RatatuiListSessions(command) => self
                .ratatui_workspace()?
                .list_sessions(command.target)
                .map_err(AppError::from),
            Command::RatatuiNodeServer(command) => self
                .ratatui_node_server(command)
                .and_then(|runtime| runtime.run().map_err(AppError::from))
                .map_err(AppError::from),
            Command::RatatuiClient(command) => self
                .ratatui_client(command)
                .and_then(|runtime| runtime.run().map_err(AppError::from))
                .map_err(AppError::from),
            Command::Help(help) => {
                print_banner();
                println!("{help}");
                Ok(())
            }
            Command::Version => {
                let full =
                    option_env!("WAITAGENT_VERSION_FULL").unwrap_or(env!("CARGO_PKG_VERSION"));
                println!("waitagent {full}");
                Ok(())
            }
            Command::ShowErrorLog => {
                let entries = crate::infra::error_log::ERROR_LOG.entries();
                if entries.is_empty() {
                    println!("(no error log entries)");
                } else {
                    for (ts, msg) in &entries {
                        let secs = ts / 1000;
                        let millis = ts % 1000;
                        println!("[{}.{:03}] {}", secs, millis, msg);
                    }
                }
                Ok(())
            }
            other => Err(AppError::from(crate::lifecycle::LifecycleError::Protocol(
                format!("tmux-backed command {other:?} is no longer supported"),
            ))),
        }
    }

    fn ratatui_workspace(&self) -> Result<RatatuiWorkspaceRuntime, AppError> {
        RatatuiWorkspaceRuntime::from_build_env_with_network(self.network.clone())
            .map_err(AppError::from)
    }

    fn ratatui_node_server(
        &self,
        _command: RatatuiNodeServerCommand,
    ) -> Result<RatatuiNodeRuntime, AppError> {
        RatatuiNodeRuntime::from_network(self.network.clone()).map_err(AppError::from)
    }

    fn ratatui_client(
        &self,
        _command: RatatuiClientCommand,
    ) -> Result<RatatuiClientRuntime, AppError> {
        RatatuiClientRuntime::from_port(self.network.port, self.network.clone())
            .map_err(AppError::from)
    }
}
