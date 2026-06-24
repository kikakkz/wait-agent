use crate::cli::RemoteNetworkConfig;
use crate::infra::tmux::{
    EmbeddedTmuxBackend, TmuxError, TmuxLayoutGateway, TmuxSocketName, TmuxWorkspaceHandle,
};

const WAITAGENT_NETWORK_PORT_OPTION: &str = "@waitagent_network_port";
const WAITAGENT_NETWORK_CONNECT_OPTION: &str = "@waitagent_network_connect";
const WAITAGENT_NETWORK_NODE_ID_OPTION: &str = "@waitagent_network_node_id";
const WAITAGENT_NETWORK_PUBLIC_ENDPOINT_OPTION: &str = "@waitagent_network_public_endpoint";

pub(crate) fn persist_workspace_network_config(
    backend: &EmbeddedTmuxBackend,
    workspace: &TmuxWorkspaceHandle,
    network: &RemoteNetworkConfig,
) -> Result<(), TmuxError> {
    persist_socket_network_config(backend, workspace.socket_name.as_str(), network)?;
    backend.set_session_option(
        workspace,
        WAITAGENT_NETWORK_PORT_OPTION,
        &network.port.to_string(),
    )?;
    backend.set_session_option(
        workspace,
        WAITAGENT_NETWORK_CONNECT_OPTION,
        network.connect.as_deref().unwrap_or(""),
    )?;
    backend.set_session_option(
        workspace,
        WAITAGENT_NETWORK_NODE_ID_OPTION,
        network.node_id.as_deref().unwrap_or(""),
    )?;
    backend.set_session_option(
        workspace,
        WAITAGENT_NETWORK_PUBLIC_ENDPOINT_OPTION,
        network.public_endpoint.as_deref().unwrap_or(""),
    )
}

pub(crate) fn persist_socket_network_config(
    backend: &EmbeddedTmuxBackend,
    socket_name: &str,
    network: &RemoteNetworkConfig,
) -> Result<(), TmuxError> {
    let socket = TmuxSocketName::new(socket_name);
    backend.set_global_option_on_socket(
        &socket,
        WAITAGENT_NETWORK_PORT_OPTION,
        &network.port.to_string(),
    )?;
    backend.set_global_option_on_socket(
        &socket,
        WAITAGENT_NETWORK_CONNECT_OPTION,
        network.connect.as_deref().unwrap_or(""),
    )?;
    backend.set_global_option_on_socket(
        &socket,
        WAITAGENT_NETWORK_NODE_ID_OPTION,
        network.node_id.as_deref().unwrap_or(""),
    )?;
    backend.set_global_option_on_socket(
        &socket,
        WAITAGENT_NETWORK_PUBLIC_ENDPOINT_OPTION,
        network.public_endpoint.as_deref().unwrap_or(""),
    )
}

pub(crate) fn recover_network_config_for_socket(
    backend: &EmbeddedTmuxBackend,
    socket_name: &str,
) -> Option<RemoteNetworkConfig> {
    let socket = TmuxSocketName::new(socket_name);
    let port = backend
        .show_global_option_on_socket(&socket, WAITAGENT_NETWORK_PORT_OPTION)
        .ok()
        .flatten()
        .and_then(|value| value.parse::<u16>().ok())?;
    Some(RemoteNetworkConfig {
        port,
        connect: backend
            .show_global_option_on_socket(&socket, WAITAGENT_NETWORK_CONNECT_OPTION)
            .ok()
            .flatten()
            .filter(|value| !value.is_empty()),
        node_id: backend
            .show_global_option_on_socket(&socket, WAITAGENT_NETWORK_NODE_ID_OPTION)
            .ok()
            .flatten()
            .filter(|value| !value.is_empty()),
        public_endpoint: backend
            .show_global_option_on_socket(&socket, WAITAGENT_NETWORK_PUBLIC_ENDPOINT_OPTION)
            .ok()
            .flatten()
            .filter(|value| !value.is_empty()),
    })
}

pub(crate) fn recover_network_config_for_workspace(
    backend: &EmbeddedTmuxBackend,
    workspace: &TmuxWorkspaceHandle,
) -> Option<RemoteNetworkConfig> {
    recover_network_config_for_socket(backend, workspace.socket_name.as_str()).or_else(|| {
        let port = backend
            .show_session_option(workspace, WAITAGENT_NETWORK_PORT_OPTION)
            .ok()
            .flatten()
            .and_then(|value| value.parse::<u16>().ok())?;
        Some(RemoteNetworkConfig {
            port,
            connect: backend
                .show_session_option(workspace, WAITAGENT_NETWORK_CONNECT_OPTION)
                .ok()
                .flatten()
                .filter(|value| !value.is_empty()),
            node_id: backend
                .show_session_option(workspace, WAITAGENT_NETWORK_NODE_ID_OPTION)
                .ok()
                .flatten()
                .filter(|value| !value.is_empty()),
            public_endpoint: backend
                .show_session_option(workspace, WAITAGENT_NETWORK_PUBLIC_ENDPOINT_OPTION)
                .ok()
                .flatten()
                .filter(|value| !value.is_empty()),
        })
    })
}

pub(crate) fn command_network_config(
    explicit_network: RemoteNetworkConfig,
    network_explicit: bool,
    command: &crate::cli::Command,
) -> RemoteNetworkConfig {
    if network_explicit {
        return explicit_network;
    }
    let Ok(backend) = EmbeddedTmuxBackend::from_build_env() else {
        return explicit_network;
    };
    recover_network_config_for_command(&backend, command).unwrap_or(explicit_network)
}

fn recover_network_config_for_command(
    backend: &EmbeddedTmuxBackend,
    command: &crate::cli::Command,
) -> Option<RemoteNetworkConfig> {
    use crate::cli::Command;

    match command {
        Command::ChromeRefreshSocket(command) | Command::ChromeRefreshSocketSignal(command) => {
            recover_network_config_for_socket(backend, &command.socket_name)
        }
        Command::UiSidebar(command)
        | Command::UiFooter(command)
        | Command::ChromeRefreshSignal(command) => recover_network_config_for_workspace(
            backend,
            &workspace_handle(&command.socket_name, &command.session_name),
        ),
        Command::SocketLifecycleHook(command) => command
            .session_name
            .as_deref()
            .filter(|session_name| !session_name.is_empty())
            .and_then(|session_name| {
                recover_network_config_for_workspace(
                    backend,
                    &workspace_handle(&command.socket_name, session_name),
                )
            })
            .or_else(|| recover_network_config_for_socket(backend, &command.socket_name)),
        Command::LocalTargetHost(command) => {
            recover_network_config_for_socket(backend, &command.socket_name)
        }
        Command::LocalTargetExited(command) => {
            recover_network_config_for_socket(backend, &command.socket_name)
        }
        Command::RemoteMainSlot(command) => recover_network_config_for_workspace(
            backend,
            &workspace_handle(&command.socket_name, &command.session_name),
        ),
        Command::RemoteServerConsole(command) => {
            recover_network_config_for_socket(backend, &command.socket_name)
        }
        Command::RemoteAuthorityTargetHost(command) => {
            recover_network_config_for_socket(backend, &command.socket_name)
        }
        Command::RemoteAuthorityOutputPump(command) => {
            recover_network_config_for_socket(backend, &command.socket_name)
        }
        Command::RemoteAuthorityPaneDied(_) => None,
        Command::RemoteTargetPublicationServer(command) => {
            recover_network_config_for_socket(backend, &command.socket_name)
        }
        Command::RemoteTargetPublicationAgent(command) => {
            recover_network_config_for_socket(backend, &command.socket_name)
        }
        Command::RemoteTargetPublicationSender(command) => {
            recover_network_config_for_socket(backend, &command.socket_name)
        }
        Command::RemoteTargetPublicationOwner(command) => {
            recover_network_config_for_socket(backend, &command.socket_name)
        }
        Command::RemoteSessionSyncOwner(command) => {
            recover_network_config_for_socket(backend, &command.socket_name)
        }
        Command::RemoteNodeIngressServer(command) => {
            recover_network_config_for_socket(backend, &command.socket_name)
        }
        Command::RemoteDaemon(_command) => {
            // RemoteDaemon is started headlessly by SSH bootstrap and receives
            // network config via explicit global CLI args.
            None
        }
        Command::RemoteRuntimeOwner(_command) => {
            // RemoteRuntimeOwner is scoped by listener_addr, not by socket_name.
            // Network config is passed via global CLI args (prepend_global_network_args).
            None
        }
        Command::RemoteTargetBindPublication(command) => {
            recover_network_config_for_socket(backend, &command.socket_name)
        }
        Command::RemoteTargetUnbindPublication(command) => {
            recover_network_config_for_socket(backend, &command.socket_name)
        }
        Command::RemoteTargetReconcilePublications(command) => {
            recover_network_config_for_socket(backend, &command.socket_name)
        }
        Command::ActivateTarget(command) => recover_network_config_for_workspace(
            backend,
            &workspace_handle(&command.current_socket_name, &command.current_session_name),
        ),
        Command::NewTarget(command) => recover_network_config_for_workspace(
            backend,
            &workspace_handle(&command.current_socket_name, &command.current_session_name),
        ),
        Command::NewSelectedRemoteSession(command) => recover_network_config_for_workspace(
            backend,
            &workspace_handle(&command.current_socket_name, &command.current_session_name),
        ),
        Command::ConnectRemoteHostPane(command) => recover_network_config_for_workspace(
            backend,
            &workspace_handle(&command.current_socket_name, &command.current_session_name),
        ),
        Command::ConnectRemoteHost(command) => recover_network_config_for_workspace(
            backend,
            &workspace_handle(&command.current_socket_name, &command.current_session_name),
        ),
        Command::MainPaneDied(command) => recover_network_config_for_workspace(
            backend,
            &workspace_handle(&command.socket_name, &command.session_name),
        ),
        Command::RemoteTargetExited(command) => recover_network_config_for_workspace(
            backend,
            &workspace_handle(&command.socket_name, &command.session_name),
        ),
        Command::FooterMenu(command) => recover_network_config_for_workspace(
            backend,
            &workspace_handle(&command.socket_name, &command.session_name),
        ),
        Command::ToggleFullscreen(command) => recover_network_config_for_workspace(
            backend,
            &workspace_handle(&command.socket_name, &command.session_name),
        ),
        Command::CloseSession(command) => recover_network_config_for_workspace(
            backend,
            &workspace_handle(&command.socket_name, &command.session_name),
        ),
        Command::LayoutReconcile(command) | Command::ChromeRefresh(command) => {
            recover_network_config_for_workspace(
                backend,
                &workspace_handle(&command.socket_name, &command.session_name),
            )
        }
        Command::Workspace
        | Command::ChromeRefreshAll
        | Command::ShowErrorLog
        | Command::Attach(_)
        | Command::List
        | Command::Detach(_)
        | Command::Stop(_)
        | Command::Help(_)
        | Command::Version => None,
    }
}

fn workspace_handle(socket_name: &str, session_name: &str) -> TmuxWorkspaceHandle {
    TmuxWorkspaceHandle {
        workspace_id: crate::domain::workspace::WorkspaceInstanceId::new(session_name.to_string()),
        socket_name: TmuxSocketName::new(socket_name),
        session_name: crate::infra::tmux::TmuxSessionName::new(session_name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::DEFAULT_REMOTE_NODE_PORT;
    use crate::infra::tmux::{TmuxGateway, TmuxSessionGateway};

    #[test]
    fn default_port_constant_matches_network_default() {
        assert_eq!(
            RemoteNetworkConfig::default().port,
            DEFAULT_REMOTE_NODE_PORT
        );
    }

    #[test]
    fn main_pane_died_hidden_command_recovers_workspace_network_state() {
        let _guard = crate::test_support::integration_test_lock();
        let backend = EmbeddedTmuxBackend::from_build_env()
            .expect("vendored tmux backend should discover build env");
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time should be monotonic")
            .as_nanos();
        let workspace_dir = std::env::temp_dir().join(format!("waitagent-network-state-{nonce:x}"));
        std::fs::create_dir_all(&workspace_dir).expect("workspace dir should be created");
        let config = crate::domain::workspace::WorkspaceInstanceConfig {
            workspace_dir: workspace_dir.clone(),
            workspace_key: format!("network-state-{nonce:x}"),
            socket_name: format!("wa-test-network-state-{nonce:x}"),
            session_name: format!("waitagent-test-network-state-{nonce:x}"),
            session_role: crate::domain::workspace::WorkspaceSessionRole::WorkspaceChrome,
            initial_rows: None,
            initial_cols: None,
            initial_program: None,
        };
        let workspace = backend
            .ensure_workspace(&config)
            .expect("workspace should be created");
        let network = RemoteNetworkConfig {
            port: 17662,
            connect: Some("10.1.29.130:17662".to_string()),
            node_id: None,
            public_endpoint: Some("nat.example:17474".to_string()),
        };
        persist_workspace_network_config(&backend, &workspace, &network)
            .expect("network state should persist");

        let recovered = command_network_config(
            RemoteNetworkConfig::default(),
            false,
            &crate::cli::Command::MainPaneDied(crate::cli::MainPaneDiedCommand {
                socket_name: workspace.socket_name.as_str().to_string(),
                session_name: workspace.session_name.as_str().to_string(),
                pane_id: "%1".to_string(),
                pane_generation: None,
            }),
        );

        assert_eq!(recovered, network);
        let _ = backend.kill_server(&workspace.socket_name);
        let _ = std::fs::remove_dir_all(workspace_dir);
    }
}
