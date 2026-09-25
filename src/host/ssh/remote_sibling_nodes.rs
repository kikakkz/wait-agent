//! Enumerates sibling ratatui node servers on a remote host via SSH.
//!
//! Every node server on a host writes a marker into
//! `<temp_dir>/waitagent-ratatui-<user>/` (`<port>.sock` on Unix,
//! `<port>.port` on Windows; see `crate::platform::local_ipc`). All node
//! servers on one host share the same `node.key`/`node.crt` pair, so the TLS
//! pin stored in the host profile authenticates sibling dials on every port,
//! not only the primary one.

use crate::host::ssh::remote_host_history_store::{RemoteHostAuthProfile, RemoteHostProfile};
use crate::host::ssh::remote_host_secret_store::{
    KeyringRemoteHostSecretStore, RemoteHostSecretStore,
};
use crate::host::ssh::remote_shell::RemoteShellKind;
use crate::host::ssh::remote_ssh_executor::{
    RemoteSshExecutor, RemoteSshTarget, RusshRemoteSshExecutor,
};

/// Remote command listing the marker path of every local ratatui node
/// server, one path per line (`.../<port>.sock` on POSIX targets,
/// `...\<port>.port` on Windows targets).
fn sibling_marker_list_command(remote_shell: RemoteShellKind) -> &'static str {
    match remote_shell {
        // `ls` exits non-zero when the glob matches nothing; `|| true` keeps
        // marker-less hosts quiet instead of looking like a failure.
        RemoteShellKind::Posix => {
            r#"ls -1 "${TMPDIR:-/tmp}"/waitagent-ratatui-*/*.sock 2>/dev/null || true"#
        }
        RemoteShellKind::Windows => {
            "Get-ChildItem \"$env:TEMP\\waitagent-ratatui-*\\*.port\" -File -ErrorAction SilentlyContinue | ForEach-Object { $_.FullName }"
        }
    }
}

/// Enumerates the node server ports running on the remote host described by
/// `profile`. Returns an empty list when the host has no markers. SSH and
/// parsing failures are returned as errors so the caller can log and skip:
/// sibling enumeration is best-effort and must never fail the primary
/// connect.
pub fn list_sibling_node_ports(profile: &RemoteHostProfile) -> Result<Vec<u16>, String> {
    let remote_shell = profile.remote_shell.unwrap_or_default();
    let target = ssh_target(profile)?;
    let output = RusshRemoteSshExecutor
        .exec(&target, sibling_marker_list_command(remote_shell), None)
        .map_err(|error| error.to_string())?;
    if output.status != 0 {
        return Err(format!(
            "sibling marker listing exited with status {}",
            output.status
        ));
    }
    Ok(parse_sibling_ports(&output.stdout, remote_shell))
}

fn ssh_target(profile: &RemoteHostProfile) -> Result<RemoteSshTarget, String> {
    let ssh_password = match &profile.auth {
        RemoteHostAuthProfile::Password { password_secret_id } => {
            let Some(secret_id) = password_secret_id else {
                return Err("password auth requires a stored SSH password".to_string());
            };
            Some(
                KeyringRemoteHostSecretStore
                    .get_secret(secret_id)
                    .map_err(|error| error.to_string())?
                    .ok_or_else(|| "stored SSH password is missing".to_string())?,
            )
        }
        RemoteHostAuthProfile::Key { .. } => None,
    };
    RemoteSshTarget::from_profile(
        profile.host.clone(),
        profile.ssh_port(),
        profile.ssh_user.clone(),
        &profile.auth,
        ssh_password,
    )
    .map_err(|error| error.to_string())
}

fn parse_sibling_ports(stdout: &[u8], remote_shell: RemoteShellKind) -> Vec<u16> {
    let extension = match remote_shell {
        RemoteShellKind::Posix => ".sock",
        RemoteShellKind::Windows => ".port",
    };
    let mut ports = Vec::new();
    for line in String::from_utf8_lossy(stdout).lines() {
        let file_name = line.trim().rsplit(['/', '\\']).next().unwrap_or("");
        let Some(port) = file_name.strip_suffix(extension) else {
            continue;
        };
        if let Ok(port) = port.parse::<u16>() {
            ports.push(port);
        }
    }
    ports.sort_unstable();
    ports.dedup();
    ports
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sibling_ports_reads_unix_marker_paths() {
        let stdout =
            b"/tmp/waitagent-ratatui-1000/7474.sock\n/tmp/waitagent-ratatui-1000/7477.sock\n";
        assert_eq!(
            parse_sibling_ports(stdout, RemoteShellKind::Posix),
            vec![7474, 7477]
        );
    }

    #[test]
    fn parse_sibling_ports_reads_windows_marker_paths() {
        let stdout = "C:\\Users\\jj\\AppData\\Local\\Temp\\waitagent-ratatui-jj\\7477.port\r\n";
        assert_eq!(
            parse_sibling_ports(stdout.as_bytes(), RemoteShellKind::Windows),
            vec![7477]
        );
    }

    #[test]
    fn parse_sibling_ports_ignores_garbage_and_duplicates() {
        let stdout = b"/tmp/waitagent-ratatui-1000/7477.sock\nnot-a-port.sock\n/tmp/x/abc.sock\n/tmp/waitagent-ratatui-0/7477.sock\n";
        assert_eq!(
            parse_sibling_ports(stdout, RemoteShellKind::Posix),
            vec![7477]
        );
    }

    #[test]
    fn parse_sibling_ports_empty_output_yields_no_ports() {
        assert!(parse_sibling_ports(b"", RemoteShellKind::Posix).is_empty());
        assert!(parse_sibling_ports(b"", RemoteShellKind::Windows).is_empty());
    }
}
