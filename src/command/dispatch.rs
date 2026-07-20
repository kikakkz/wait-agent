use crate::cli::{Command, RemoteNetworkConfig};
use crate::error::AppError;
use crate::infra::tmux::{EmbeddedTmuxBackend, WaitagentSessionListEntry};
use crate::runtime::event_driven_pane_runtime::EventDrivenPaneRuntime;
use crate::runtime::footer_menu_runtime::FooterMenuRuntime;
use crate::runtime::network_state_runtime::recover_network_config_for_socket;
use crate::runtime::remote_authority_target_host_runtime::{
    run_geometry_event, run_pane_died_event, RemoteAuthorityTargetHostRuntime,
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
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

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
                    command.runtime_signal,
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
            Command::RemoteAuthorityGeometryEvent(command) => {
                run_geometry_event(command).map_err(AppError::from)
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
                let hook_name = command.hook_name.clone();
                let session_name = command.session_name.clone();
                self.remote_target_publication()?
                    .run_socket_lifecycle_hook(command)
                    .map_err(AppError::from)?;
                if hook_name.as_deref() == Some("session-closed") {
                    if let Some(session_name) = session_name.as_deref().filter(|s| !s.is_empty()) {
                        self.layout()?
                            .run_target_exited_chrome_refresh(&socket_name, session_name)
                            .map_err(AppError::from)?;
                    }
                }
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
            Command::ToggleSidebar(command) => self
                .layout()?
                .run_toggle_sidebar(command)
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
            Command::Cleanup => self.run_cleanup(),
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
        let sessions = backend
            .list_waitagent_session_entries()
            .map_err(tmux_command_error)?;
        if sessions.is_empty() {
            println!("no waitagent tmux sessions running");
            return Ok(());
        }

        let mut ports_by_socket = HashMap::new();
        for session in sessions {
            let port = ports_by_socket
                .entry(session.socket_name.clone())
                .or_insert_with(|| {
                    recover_network_config_for_socket(&backend, &session.socket_name)
                        .map(|network| network.port.to_string())
                        .unwrap_or_else(|| "-".to_string())
                });
            println!("{}", format_list_session_line(&session, &port));
        }
        Ok(())
    }

    fn run_cleanup(&self) -> Result<(), AppError> {
        let backend = EmbeddedTmuxBackend::from_build_env().map_err(tmux_command_error)?;
        let report = backend
            .cleanup_stale_waitagent_socket_files()
            .map_err(tmux_command_error)?;
        println!(
            "cleaned {} stale waitagent tmux sockets (kept {} live)",
            report.removed, report.live
        );
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

fn format_list_session_line(session: &WaitagentSessionListEntry, port: &str) -> String {
    format!(
        "{}: port {}, up {}, {} windows ({}){}",
        session.display_session_id(),
        port,
        format_uptime(session.created_at_unix_secs),
        session.window_count,
        if session.attached_clients > 0 {
            "attached"
        } else {
            "detached"
        },
        session.role_tag(),
    )
}

fn format_uptime(created_at_unix_secs: Option<u64>) -> String {
    let Some(created_at_unix_secs) = created_at_unix_secs else {
        return "-".to_string();
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(created_at_unix_secs);
    let elapsed = now.saturating_sub(created_at_unix_secs);
    format_duration(elapsed)
}

fn format_duration(seconds: u64) -> String {
    let days = seconds / 86_400;
    let hours = (seconds % 86_400) / 3_600;
    let minutes = (seconds % 3_600) / 60;
    let secs = seconds % 60;

    if days > 0 {
        format!("{days}d{hours}h")
    } else if hours > 0 {
        format!("{hours}h{minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m{secs}s")
    } else {
        format!("{secs}s")
    }
}

#[cfg(test)]
mod tests {
    use super::{format_duration, format_list_session_line};
    use crate::domain::workspace::WorkspaceSessionRole;
    use crate::infra::tmux::WaitagentSessionListEntry;

    #[test]
    fn dispatcher_module_builds_without_host_global_listener_gate() {}

    #[test]
    fn list_session_line_includes_port_uptime_and_role() {
        let session = WaitagentSessionListEntry {
            socket_name: "wa-1234".to_string(),
            session_name: "waitagent-1234".to_string(),
            attached_clients: 1,
            window_count: 2,
            created_at_unix_secs: None,
            session_role: Some(WorkspaceSessionRole::WorkspaceChrome),
        };

        assert_eq!(
            format_list_session_line(&session, "7474"),
            "1234: port 7474, up -, 2 windows (attached) [main]"
        );
    }

    #[test]
    fn list_uptime_formatter_uses_compact_units() {
        assert_eq!(format_duration(9), "9s");
        assert_eq!(format_duration(65), "1m5s");
        assert_eq!(format_duration(3_660), "1h1m");
        assert_eq!(format_duration(90_000), "1d1h");
    }
}
