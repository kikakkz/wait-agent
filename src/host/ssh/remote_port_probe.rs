use crate::host::ssh::remote_host_history_store::{RemoteHostAuthProfile, RemoteHostProfile};
use crate::host::ssh::remote_host_secret_store::{
    DefaultRemoteHostSecretStore, KeyringRemoteHostSecretStore, RemoteHostSecretStore,
    RemoteHostSecretValue,
};
use crate::host::ssh::remote_shell::RemoteShellKind;
use crate::host::ssh::remote_ssh_executor::{
    RemoteSshExecutor, RemoteSshTarget, RusshRemoteSshExecutor,
};
use crate::host::ssh::ssh_remote_host_bootstrapper::powershell_command;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemotePortProbePreference {
    Auto,
    Port(u16),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemotePortProbeResult {
    pub port: u16,
    pub reused_existing_waitagent: bool,
}

pub trait RemotePortProbe {
    type Error;

    fn choose_remote_port(
        &self,
        preference: &RemotePortProbePreference,
        local_connect_endpoint: &str,
    ) -> Result<RemotePortProbeResult, Self::Error>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemotePortProbeError {
    message: String,
}

impl RemotePortProbeError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for RemotePortProbeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for RemotePortProbeError {}

#[derive(Debug, Clone, Default)]
pub struct StaticRemotePortProbe;

impl RemotePortProbe for StaticRemotePortProbe {
    type Error = RemotePortProbeError;

    fn choose_remote_port(
        &self,
        preference: &RemotePortProbePreference,
        _local_connect_endpoint: &str,
    ) -> Result<RemotePortProbeResult, Self::Error> {
        let port = match preference {
            RemotePortProbePreference::Auto => 7474,
            RemotePortProbePreference::Port(port) => *port,
        };
        Ok(RemotePortProbeResult {
            port,
            reused_existing_waitagent: false,
        })
    }
}

#[derive(Debug, Clone)]
pub struct SshRemotePortProbe<S = DefaultRemoteHostSecretStore, E = RusshRemoteSshExecutor> {
    profile: RemoteHostProfile,
    secret_store: S,
    ssh_executor: E,
    // Detected remote shell family of the target. The probe command is
    // generated per family: POSIX `ss` loop or a PowerShell TCP-listener
    // enumeration (`docs/windows-ssh-target-design.md` task T2). Both emit
    // the same `port=N` contract.
    remote_shell: RemoteShellKind,
}

impl SshRemotePortProbe<DefaultRemoteHostSecretStore, RusshRemoteSshExecutor> {
    pub fn new(profile: RemoteHostProfile, remote_shell: RemoteShellKind) -> Self {
        Self {
            profile,
            secret_store: KeyringRemoteHostSecretStore,
            ssh_executor: RusshRemoteSshExecutor,
            remote_shell,
        }
    }
}

// TODO(cleanup): transitional remote code, kept for Phase 8 wiring.
#[allow(dead_code)]
impl<S> SshRemotePortProbe<S, RusshRemoteSshExecutor> {
    pub fn with_secret_store(profile: RemoteHostProfile, secret_store: S) -> Self {
        Self {
            profile,
            secret_store,
            ssh_executor: RusshRemoteSshExecutor,
            remote_shell: RemoteShellKind::Posix,
        }
    }
}

// TODO(cleanup): transitional remote code, kept for Phase 8 wiring.
#[allow(dead_code)]
impl<S, E> SshRemotePortProbe<S, E> {
    pub fn with_secret_store_and_executor(
        profile: RemoteHostProfile,
        secret_store: S,
        ssh_executor: E,
        remote_shell: RemoteShellKind,
    ) -> Self {
        Self {
            profile,
            secret_store,
            ssh_executor,
            remote_shell,
        }
    }

    pub fn probe_script(preference: &RemotePortProbePreference) -> String {
        match preference {
            RemotePortProbePreference::Port(port) => format!("choose_port {port}"),
            RemotePortProbePreference::Auto => "choose_port auto".to_string(),
        }
    }
}

impl<S, E> RemotePortProbe for SshRemotePortProbe<S, E>
where
    S: RemoteHostSecretStore,
    S::Error: ToString,
    E: RemoteSshExecutor,
    E::Error: ToString,
{
    type Error = RemotePortProbeError;

    fn choose_remote_port(
        &self,
        preference: &RemotePortProbePreference,
        _local_connect_endpoint: &str,
    ) -> Result<RemotePortProbeResult, Self::Error> {
        let ssh_password = self.ssh_password()?;
        let target = RemoteSshTarget::from_profile(
            self.profile.host.clone(),
            self.profile.ssh_port(),
            self.profile.ssh_user.clone(),
            &self.profile.auth,
            ssh_password,
        )
        .map_err(|error| RemotePortProbeError::new(error.to_string()))?;
        let output = self
            .ssh_executor
            .exec(&target, &self.probe_command(preference), None)
            .map_err(|error| RemotePortProbeError::new(error.to_string()))?;
        if output.status != 0 {
            return Err(RemotePortProbeError::new(format!(
                "remote port probe failed with status {}{}",
                output.status,
                stderr_summary(&output.stderr)
            )));
        }
        parse_probe_output(&String::from_utf8_lossy(&output.stdout))
    }
}

impl<S, E> SshRemotePortProbe<S, E>
where
    S: RemoteHostSecretStore,
    S::Error: ToString,
{
    fn probe_command(&self, preference: &RemotePortProbePreference) -> String {
        match self.remote_shell {
            RemoteShellKind::Posix => remote_probe_command(preference),
            RemoteShellKind::Windows => windows_remote_probe_command(preference),
        }
    }

    fn ssh_password(&self) -> Result<Option<RemoteHostSecretValue>, RemotePortProbeError> {
        let RemoteHostAuthProfile::Password { password_secret_id } = &self.profile.auth else {
            return Ok(None);
        };
        let Some(secret_id) = password_secret_id else {
            return Err(RemotePortProbeError::new(
                "password auth requires a saved SSH password secret id for remote port probe",
            ));
        };
        self.secret_store
            .get_secret(secret_id)
            .map_err(|error| RemotePortProbeError::new(error.to_string()))?
            .ok_or_else(|| {
                RemotePortProbeError::new(format!(
                    "SSH password secret `{}` was not found for remote port probe",
                    secret_id.as_str()
                ))
            })
            .map(Some)
    }
}

fn stderr_summary(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let text = text.trim();
    if text.is_empty() {
        String::new()
    } else {
        format!(": {text}")
    }
}

pub fn remote_probe_command(preference: &RemotePortProbePreference) -> String {
    let candidate_expr = match preference {
        RemotePortProbePreference::Auto => "$(seq 7474 7574)".to_string(),
        RemotePortProbePreference::Port(port) => port.to_string(),
    };
    format!(
        r#"for p in {candidate_expr}; do if ! ss -ltn 2>/dev/null | grep -q ":$p"; then echo port=$p; exit 0; fi; done; echo no_port; exit 1"#
    )
}

/// PowerShell port probe for Windows targets: enumerate the active TCP
/// listeners once and pick the first candidate port not in use. Emits the
/// same `port=N` contract as the POSIX probe
/// (`docs/windows-ssh-target-design.md` §2).
fn windows_remote_probe_command(preference: &RemotePortProbePreference) -> String {
    let candidates = match preference {
        RemotePortProbePreference::Auto => "7474..7574".to_string(),
        RemotePortProbePreference::Port(port) => format!("@({port})"),
    };
    let script = format!(
        "$inUse = @([Net.NetworkInformation.IPGlobalProperties]::GetIPGlobalProperties().GetActiveTcpListeners() | ForEach-Object {{ $_.Port }}); \
foreach ($p in {candidates}) {{ if ($inUse -notcontains $p) {{ Write-Output \"port=$p\"; exit 0 }} }}; \
Write-Output 'no_port'; exit 1"
    );
    powershell_command(&script)
}

fn parse_probe_output(output: &str) -> Result<RemotePortProbeResult, RemotePortProbeError> {
    for line in output.lines() {
        let line = line.trim();
        if let Some(raw) = line.strip_prefix("port=") {
            let port = raw.parse::<u16>().map_err(|_| {
                RemotePortProbeError::new("remote port probe returned invalid port")
            })?;
            return Ok(RemotePortProbeResult {
                port,
                reused_existing_waitagent: false,
            });
        }
        if let Some(raw) = line.strip_prefix("reuse=") {
            let port = raw.parse::<u16>().map_err(|_| {
                RemotePortProbeError::new("remote port probe returned invalid reused port")
            })?;
            return Ok(RemotePortProbeResult {
                port,
                reused_existing_waitagent: true,
            });
        }
    }
    Err(RemotePortProbeError::new(
        "remote port probe did not return a usable port",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::ssh::remote_ssh_executor::{
        RemoteSshExecutor, RemoteSshOutput, RemoteSshTarget,
    };
    use std::cell::RefCell;
    use std::rc::Rc;

    type SshCallLog = Vec<(RemoteSshTarget, String, Option<String>)>;

    #[derive(Clone)]
    struct RecordingSshExecutor {
        calls: Rc<RefCell<SshCallLog>>,
        output: RemoteSshOutput,
    }

    impl RemoteSshExecutor for RecordingSshExecutor {
        type Error = String;

        fn exec(
            &self,
            target: &RemoteSshTarget,
            command: &str,
            stdin: Option<&str>,
        ) -> Result<RemoteSshOutput, Self::Error> {
            self.calls.borrow_mut().push((
                target.clone(),
                command.to_string(),
                stdin.map(str::to_string),
            ));
            Ok(self.output.clone())
        }
    }

    #[test]
    fn remote_host_port_probe_parses_free_and_reused_ports() {
        assert_eq!(
            parse_probe_output("port=7476\n").unwrap(),
            RemotePortProbeResult {
                port: 7476,
                reused_existing_waitagent: false,
            }
        );
        assert_eq!(
            parse_probe_output("reuse=7474\n").unwrap(),
            RemotePortProbeResult {
                port: 7474,
                reused_existing_waitagent: true,
            }
        );
    }

    #[test]
    fn remote_host_port_probe_requires_saved_password_secret_for_password_auth() {
        use crate::host::ssh::remote_host_secret_store::MemoryRemoteHostSecretStore;
        let profile = RemoteHostProfile {
            name: "130".to_string(),
            host: "10.1.29.130".to_string(),
            ssh_user: "kk".to_string(),
            auth: RemoteHostAuthProfile::Password {
                password_secret_id: None,
            },
            sudo_password_secret_id: None,
            preferred_remote_port:
                crate::host::ssh::remote_host_history_store::RemotePortPreference::Auto,
            last_remote_port: None,
            last_endpoint: None,
            last_connected_at: None,
            use_install_proxy: true,
            ..RemoteHostProfile::default()
        };
        let probe =
            SshRemotePortProbe::with_secret_store(profile, MemoryRemoteHostSecretStore::default());

        let error = probe
            .ssh_password()
            .expect_err("password auth without secret id should fail before spawning ssh");

        assert!(error.to_string().contains("saved SSH password secret id"));
    }

    #[test]
    fn remote_host_port_probe_command_prefers_7474_for_auto() {
        let command = remote_probe_command(&RemotePortProbePreference::Auto);
        assert!(command.contains("seq 7474 7574"));
        assert!(command.contains("ss -ltn"));
    }

    #[test]
    fn remote_host_port_probe_windows_command_enumerates_tcp_listeners() {
        use crate::host::ssh::ssh_remote_host_bootstrapper::decode_powershell_command;
        let command = windows_remote_probe_command(&RemotePortProbePreference::Auto);

        let script = decode_powershell_command(&command);
        assert!(script.contains("GetActiveTcpListeners"));
        assert!(script.contains("7474..7574"));
        assert!(script.contains("port="));
        assert!(script.contains("exit 0"));
        assert!(script.contains("exit 1"));
        assert!(!script.contains("ss -ltn"));

        let fixed = windows_remote_probe_command(&RemotePortProbePreference::Port(7476));
        assert!(decode_powershell_command(&fixed).contains("@(7476)"));
    }

    #[test]
    fn remote_host_port_probe_dispatches_powershell_command_for_windows_shell() {
        use crate::host::ssh::remote_host_secret_store::{
            MemoryRemoteHostSecretStore, RemoteHostSecretId, RemoteHostSecretValue,
        };
        use crate::host::ssh::ssh_remote_host_bootstrapper::decode_powershell_command;
        let ssh_id = RemoteHostSecretId::new("waitagent.remote-host.win.ssh-password").unwrap();
        let store = MemoryRemoteHostSecretStore::default();
        store
            .put_secret(&ssh_id, RemoteHostSecretValue::new("ssh-secret"))
            .unwrap();
        let profile = RemoteHostProfile {
            name: "win".to_string(),
            host: "192.168.1.6".to_string(),
            ssh_user: "jj".to_string(),
            auth: RemoteHostAuthProfile::Password {
                password_secret_id: Some(ssh_id),
            },
            sudo_password_secret_id: None,
            preferred_remote_port:
                crate::host::ssh::remote_host_history_store::RemotePortPreference::Auto,
            last_remote_port: None,
            last_endpoint: None,
            last_connected_at: None,
            use_install_proxy: true,
            ..RemoteHostProfile::default()
        };
        let calls = Rc::new(RefCell::new(Vec::new()));
        let executor = RecordingSshExecutor {
            calls: calls.clone(),
            output: RemoteSshOutput {
                status: 0,
                stdout: b"port=7475\n".to_vec(),
                stderr: Vec::new(),
            },
        };
        let probe = SshRemotePortProbe::with_secret_store_and_executor(
            profile,
            store,
            executor,
            RemoteShellKind::Windows,
        );

        let result = probe
            .choose_remote_port(&RemotePortProbePreference::Auto, "10.1.26.84:7474")
            .unwrap();

        assert_eq!(result.port, 7475);
        let calls = calls.borrow();
        assert_eq!(calls.len(), 1);
        let script = decode_powershell_command(&calls[0].1);
        assert!(script.contains("GetActiveTcpListeners"));
        assert!(calls[0]
            .1
            .starts_with("powershell -NoProfile -NonInteractive -EncodedCommand "));
        assert!(!script.contains("ss -ltn"));
    }
    #[test]
    fn remote_host_port_probe_uses_in_process_ssh_executor() {
        use crate::host::ssh::remote_host_secret_store::{
            MemoryRemoteHostSecretStore, RemoteHostSecretId, RemoteHostSecretValue,
        };
        let ssh_id = RemoteHostSecretId::new("waitagent.remote-host.130.ssh-password").unwrap();
        let store = MemoryRemoteHostSecretStore::default();
        store
            .put_secret(&ssh_id, RemoteHostSecretValue::new("ssh-secret"))
            .unwrap();
        let profile = RemoteHostProfile {
            name: "130".to_string(),
            host: "10.1.29.130".to_string(),
            ssh_user: "kk".to_string(),
            auth: RemoteHostAuthProfile::Password {
                password_secret_id: Some(ssh_id),
            },
            sudo_password_secret_id: None,
            preferred_remote_port:
                crate::host::ssh::remote_host_history_store::RemotePortPreference::Auto,
            last_remote_port: None,
            last_endpoint: None,
            last_connected_at: None,
            use_install_proxy: true,
            ..RemoteHostProfile::default()
        };
        let calls = Rc::new(RefCell::new(Vec::new()));
        let executor = RecordingSshExecutor {
            calls: calls.clone(),
            output: RemoteSshOutput {
                status: 0,
                stdout: b"port=7475\n".to_vec(),
                stderr: Vec::new(),
            },
        };
        let probe = SshRemotePortProbe::with_secret_store_and_executor(
            profile,
            store,
            executor,
            RemoteShellKind::Posix,
        );

        let result = probe
            .choose_remote_port(&RemotePortProbePreference::Auto, "10.1.26.84:7474")
            .unwrap();

        assert_eq!(result.port, 7475);
        let calls = calls.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0.host, "10.1.29.130");
        assert_eq!(calls[0].0.user, "kk");
        assert!(calls[0].1.contains("seq 7474 7574"));
        assert_eq!(calls[0].2, None);
    }
}
