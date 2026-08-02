use crate::cli::{Command, RemoteNetworkConfig};

/// Returns the explicitly-provided network configuration.
///
/// Previously this function recovered network state from tmux session/global
/// options. During the ratatui migration network config is passed exclusively
/// via global CLI args, so no tmux-backed recovery is performed.
pub(crate) fn command_network_config(
    explicit_network: RemoteNetworkConfig,
    _network_explicit: bool,
    _command: &Command,
) -> RemoteNetworkConfig {
    explicit_network
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::default_remote_node_port;

    #[test]
    fn default_port_constant_matches_network_default() {
        assert_eq!(
            RemoteNetworkConfig::default().port,
            default_remote_node_port()
        );
    }
}

use crate::infra::tmux::{EmbeddedTmuxBackend, TmuxSocketName};

/// Stub preserved for remote runtime compatibility.
///
/// The tmux backend no longer exists, so socket-level network recovery always
/// returns `None`. This function is unreachable in the ratatui-only default
/// path and will be removed in a later phase.
pub(crate) fn recover_network_config_for_socket(
    _backend: &EmbeddedTmuxBackend,
    _socket_name: &str,
) -> Option<RemoteNetworkConfig> {
    None
}

/// Stub preserved for remote runtime compatibility.
pub(crate) fn recover_network_config_for_workspace(
    _backend: &EmbeddedTmuxBackend,
    _socket_name: &str,
    _session_name: &str,
) -> Option<RemoteNetworkConfig> {
    None
}

pub(crate) fn workspace_handle(
    socket_name: &str,
    session_name: &str,
) -> crate::infra::tmux::TmuxWorkspaceHandle {
    crate::infra::tmux::TmuxWorkspaceHandle {
        workspace_id: crate::domain::workspace::WorkspaceInstanceId::new(session_name.to_string()),
        socket_name: TmuxSocketName::new(socket_name),
        session_name: crate::infra::tmux::TmuxSessionName::new(session_name),
    }
}
