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
