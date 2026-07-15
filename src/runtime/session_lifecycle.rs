use crate::cli::{prepend_global_network_args, RemoteNetworkConfig};

/// Builds the tmux `run-shell` command string for the global `session-closed`
/// (and other session lifecycle) hook.
///
/// This is shared between workspace bootstrap (so pure local targets also get
/// the hook) and the remote publication compatibility path (so existing
/// publication bindings keep working).
pub fn session_lifecycle_hook_tmux_command(
    executable: &str,
    socket_name: &str,
    network: &RemoteNetworkConfig,
) -> String {
    let hook_command = std::iter::once(executable.to_string())
        .chain(prepend_global_network_args(
            vec![
                "__socket-lifecycle-hook".to_string(),
                "--socket-name".to_string(),
                socket_name.to_string(),
                "--hook-name".to_string(),
                "#{hook}".to_string(),
                "--session-name".to_string(),
                "#{hook_session_name}".to_string(),
            ],
            network,
        ))
        .map(|arg| shell_escape(&arg))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "run-shell -b {}",
        tmux_quote_argument(&format!("{hook_command} >/dev/null 2>&1"))
    )
}

fn shell_escape(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn tmux_quote_argument(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(test)]
mod tests {
    use super::session_lifecycle_hook_tmux_command;
    use crate::cli::RemoteNetworkConfig;

    #[test]
    fn hook_command_targets_socket_lifecycle_hook_with_hook_session_name() {
        let command = session_lifecycle_hook_tmux_command(
            "/tmp/wait agent",
            "wa-local",
            &RemoteNetworkConfig::default(),
        );
        assert!(command.starts_with("run-shell -b "));
        assert!(command.contains("__socket-lifecycle-hook"));
        assert!(command.contains("--socket-name"));
        assert!(command.contains("wa-local"));
        assert!(command.contains("--hook-name"));
        assert!(command.contains("--session-name"));
        assert!(command.contains("#{hook_session_name}"));
        assert!(command.contains(">/dev/null 2>&1"));
    }
}
