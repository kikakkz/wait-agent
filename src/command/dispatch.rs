use crate::cli::{Command, RemoteNetworkConfig};
use crate::error::AppError;
use crate::infra::tmux::EmbeddedTmuxBackend;
use crate::runtime::event_driven_pane_runtime::EventDrivenPaneRuntime;
use crate::runtime::footer_menu_runtime::FooterMenuRuntime;
use crate::runtime::remote_authority_target_host_runtime::{
    run_pane_died_event, RemoteAuthorityTargetHostRuntime,
};
use crate::runtime::remote_host::connect_remote_host_pane_runtime::ConnectRemoteHostPaneRuntime;
use crate::runtime::remote_main_slot_ingress_runtime::RemoteMainSlotIngressRuntime;
use crate::runtime::remote_node_ingress_server_runtime::RemoteNodeIngressServerRuntime;
use crate::runtime::remote_node_session_owner_runtime::RemoteNodeSessionOwnerRuntime;
use crate::runtime::remote_node_session_sync_runtime::RemoteNodeSessionSyncRuntime;
use crate::runtime::remote_runtime_owner_runtime::RemoteRuntimeOwnerRuntime;
use crate::runtime::remote_server_console_runtime::RemoteServerConsoleRuntime;
use crate::runtime::remote_target_publication_runtime::RemoteTargetPublicationRuntime;
use crate::runtime::workspace_command_runtime::WorkspaceCommandRuntime;
use crate::runtime::workspace_layout_runtime::WorkspaceLayoutRuntime;
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
                .workspace()?
                .run_workspace_entry()
                .map_err(AppError::from),
            Command::ChromeRefreshOwner(command) => self
                .layout()?
                .run_chrome_refresh_owner(command)
                .map_err(AppError::from),
            Command::ChromeRefreshSocket(command) => self
                .layout()?
                .run_chrome_refresh_on_socket(&command.socket_name)
                .map_err(AppError::from),
            Command::ChromeRefreshSocketSignal(command) => self
                .workspace()?
                .signal_runtime_command_changed(
                    &command.socket_name,
                    command.target_session_name.as_deref(),
                    command.command_name.as_deref(),
                    command.event_seq,
                )
                .map_err(AppError::from),
            Command::UiSidebar(command) => {
                self.pane()?.run_sidebar(command).map_err(AppError::from)
            }
            Command::UiFooter(command) => self.pane()?.run_footer(command).map_err(AppError::from),
            Command::LocalTargetHost(command) => self
                .workspace()?
                .run_local_target_host(command)
                .map_err(AppError::from),
            Command::LocalTargetExited(command) => self
                .workspace()?
                .run_local_target_exited(command)
                .map_err(AppError::from),
            Command::RemoteMainSlot(command) => self
                .remote_main_slot_ingress()?
                .run(command)
                .map_err(AppError::from),
            Command::RemoteServerConsole(command) => self
                .remote_server_console()?
                .run(command)
                .map_err(AppError::from),
            Command::RemoteAuthorityTargetHost(command) => self
                .remote_authority_target_host()?
                .run_target_host(command)
                .map_err(AppError::from),
            Command::RemoteAuthorityOutputPump(command) => self
                .remote_authority_target_host()?
                .run_output_pump(command)
                .map_err(AppError::from),
            Command::RemoteAuthorityPaneDied(command) => {
                run_pane_died_event(command).map_err(AppError::from)
            }
            Command::RemoteTargetPublicationServer(command) => self
                .remote_target_publication()?
                .run_publication_server(command)
                .map_err(AppError::from),
            Command::RemoteTargetPublicationAgent(command) => self
                .remote_target_publication()?
                .run_publication_agent(command)
                .map_err(AppError::from),
            Command::RemoteTargetPublicationSender(command) => self
                .remote_node_session_owner()?
                .run_publication_sender(command)
                .map_err(AppError::from),
            Command::RemoteTargetPublicationOwner(command) => self
                .remote_target_publication()?
                .run_publication_owner(command)
                .map_err(AppError::from),
            Command::RemoteSessionSyncOwner(command) => {
                RemoteNodeSessionSyncRuntime::run_owner(command, self.network.clone())
                    .map_err(AppError::from)
            }
            Command::RemoteNodeIngressServer(command) => self
                .remote_node_ingress(command.socket_name)?
                .run_owner(command.ready_socket.as_deref())
                .map_err(AppError::from),
            Command::RemoteDaemon(_) => self
                .workspace()?
                .run_remote_daemon()
                .map_err(AppError::from),
            Command::RemoteRuntimeOwner(command) => self
                .remote_runtime_owner()?
                .run_owner(command)
                .map_err(AppError::from),
            Command::SocketLifecycleHook(command) => {
                let socket_name = command.socket_name.clone();
                self.remote_target_publication()?
                    .run_socket_lifecycle_hook(command)
                    .map_err(AppError::from)?;
                self.layout()?
                    .run_chrome_refresh_on_socket(&socket_name)
                    .map_err(AppError::from)
            }
            Command::RemoteTargetBindPublication(command) => self
                .remote_target_publication()?
                .run_bind_publication(command)
                .map_err(AppError::from),
            Command::RemoteTargetUnbindPublication(command) => self
                .remote_target_publication()?
                .run_unbind_publication(command)
                .map_err(AppError::from),
            Command::RemoteTargetReconcilePublications(command) => self
                .remote_target_publication()?
                .run_reconcile_publications(command)
                .map_err(AppError::from),
            Command::ActivateTarget(command) => self
                .workspace()?
                .run_activate_target(command)
                .map_err(AppError::from),
            Command::NewTarget(command) => self
                .workspace()?
                .run_new_target(command)
                .map_err(AppError::from),
            Command::NewSelectedRemoteSession(command) => self
                .workspace()?
                .run_new_selected_remote_session(command)
                .map_err(AppError::from),
            Command::ConnectRemoteHost(command) => self
                .workspace()?
                .run_connect_remote_host(command)
                .map_err(AppError::from),
            Command::ConnectRemoteHostPane(command) => self
                .connect_remote_host_pane()
                .run(command)
                .map_err(AppError::from),
            Command::MainPaneDied(command) => self
                .workspace()?
                .run_main_pane_died(command)
                .map_err(AppError::from),
            Command::RemoteTargetExited(command) => self
                .workspace()?
                .run_remote_target_exited(command)
                .map_err(AppError::from),
            Command::FooterMenu(command) => {
                self.footer_menu()?.run(command).map_err(AppError::from)
            }
            Command::ToggleFullscreen(command) => self
                .workspace()?
                .run_toggle_fullscreen(command)
                .map_err(AppError::from),
            Command::CloseSession(command) => self
                .layout()?
                .run_close_session(command)
                .map_err(AppError::from),
            Command::LayoutReconcile(command) => self
                .layout()?
                .run_reconcile(command)
                .map_err(AppError::from),
            Command::ChromeRefresh(command) => self
                .layout()?
                .run_chrome_refresh(command)
                .map_err(AppError::from),
            Command::ChromeRefreshSignal(command) => self
                .layout()?
                .run_chrome_refresh_signal(command)
                .map_err(AppError::from),
            Command::MainPaneOutputEventBridge(command) => self
                .workspace()?
                .run_main_pane_output_event_bridge(command)
                .map_err(AppError::from),
            Command::ChromeRefreshAll => self
                .layout()?
                .run_chrome_refresh_all()
                .map_err(AppError::from),
            Command::Attach(command) => self
                .workspace()?
                .run_attach(command)
                .map_err(AppError::from),
            Command::List => self.run_list(),
            Command::Detach(command) => self
                .workspace()?
                .run_detach(command)
                .map_err(AppError::from),
            Command::Stop(command) => self.workspace()?.run_stop(command).map_err(AppError::from),
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
        }
    }

    fn workspace(&self) -> Result<WorkspaceCommandRuntime, AppError> {
        WorkspaceCommandRuntime::from_build_env_with_network(self.network.clone())
            .map_err(AppError::from)
    }

    fn run_list(&self) -> Result<(), AppError> {
        let backend = EmbeddedTmuxBackend::from_build_env().map_err(tmux_command_error)?;
        let sessions = crate::application::session_service::SessionService::new(backend)
            .list_sessions()
            .map_err(tmux_command_error)?;
        if sessions.is_empty() {
            println!("no waitagent tmux sessions running");
            return Ok(());
        }

        for session in sessions {
            println!("{}", session.summary_line());
        }
        Ok(())
    }

    fn pane(&self) -> Result<EventDrivenPaneRuntime, AppError> {
        EventDrivenPaneRuntime::from_build_env_with_network(self.network.clone())
            .map_err(AppError::from)
    }

    fn footer_menu(&self) -> Result<FooterMenuRuntime, AppError> {
        FooterMenuRuntime::from_build_env_with_network(self.network.clone()).map_err(AppError::from)
    }

    fn connect_remote_host_pane(&self) -> ConnectRemoteHostPaneRuntime {
        ConnectRemoteHostPaneRuntime::new(self.network.clone())
    }

    fn remote_authority_target_host(&self) -> Result<RemoteAuthorityTargetHostRuntime, AppError> {
        RemoteAuthorityTargetHostRuntime::from_build_env(self.network.clone())
            .map_err(AppError::from)
    }

    fn remote_main_slot_ingress(&self) -> Result<RemoteMainSlotIngressRuntime, AppError> {
        RemoteMainSlotIngressRuntime::from_build_env_with_network(self.network.clone())
            .map_err(AppError::from)
    }

    fn remote_node_session_owner(&self) -> Result<RemoteNodeSessionOwnerRuntime, AppError> {
        RemoteNodeSessionOwnerRuntime::from_build_env_with_network(self.network.clone())
            .map_err(AppError::from)
    }

    fn remote_node_ingress(
        &self,
        socket_name: impl Into<String>,
    ) -> Result<RemoteNodeIngressServerRuntime, AppError> {
        RemoteNodeIngressServerRuntime::from_build_env_with_network_and_socket(
            self.network.clone(),
            socket_name,
        )
        .map_err(AppError::from)
    }

    fn remote_runtime_owner(&self) -> Result<RemoteRuntimeOwnerRuntime, AppError> {
        RemoteRuntimeOwnerRuntime::from_build_env_with_network(self.network.clone())
            .map_err(AppError::from)
    }

    fn remote_server_console(&self) -> Result<RemoteServerConsoleRuntime, AppError> {
        RemoteServerConsoleRuntime::from_build_env_with_network(self.network.clone())
            .map_err(AppError::from)
    }

    fn remote_target_publication(&self) -> Result<RemoteTargetPublicationRuntime, AppError> {
        RemoteTargetPublicationRuntime::from_build_env_with_network(self.network.clone())
            .map_err(AppError::from)
    }

    fn layout(&self) -> Result<WorkspaceLayoutRuntime, AppError> {
        WorkspaceLayoutRuntime::from_build_env_with_network(self.network.clone())
            .map_err(AppError::from)
    }
}

fn tmux_command_error(error: crate::infra::tmux::TmuxError) -> AppError {
    AppError::from(crate::lifecycle::LifecycleError::Io(
        "tmux-native waitagent command failed".to_string(),
        std::io::Error::new(std::io::ErrorKind::Other, error.to_string()),
    ))
}

#[cfg(test)]
mod tests {
    #[test]
    fn dispatcher_module_builds_without_host_global_listener_gate() {}
}
