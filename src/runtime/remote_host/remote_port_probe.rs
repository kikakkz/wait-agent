#![allow(dead_code)]

use crate::runtime::remote_host::remote_host_history_store::{
    RemoteHostAuthProfile, RemoteHostProfile,
};
use crate::runtime::remote_host::remote_host_secret_store::{
    FileRemoteHostSecretStore, RemoteHostSecretStore, RemoteHostSecretValue,
};
use std::fmt;
use std::process::{Command, Stdio};

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
pub struct SshRemotePortProbe<S = FileRemoteHostSecretStore> {
    profile: RemoteHostProfile,
    secret_store: S,
}

impl SshRemotePortProbe<FileRemoteHostSecretStore> {
    pub fn new(profile: RemoteHostProfile) -> Self {
        Self {
            profile,
            secret_store: FileRemoteHostSecretStore::default(),
        }
    }
}

impl<S> SshRemotePortProbe<S> {
    pub fn with_secret_store(profile: RemoteHostProfile, secret_store: S) -> Self {
        Self {
            profile,
            secret_store,
        }
    }

    pub fn probe_script(preference: &RemotePortProbePreference) -> String {
        match preference {
            RemotePortProbePreference::Port(port) => format!("choose_port {port}"),
            RemotePortProbePreference::Auto => "choose_port auto".to_string(),
        }
    }
}

impl<S> RemotePortProbe for SshRemotePortProbe<S>
where
    S: RemoteHostSecretStore,
    S::Error: ToString,
{
    type Error = RemotePortProbeError;

    fn choose_remote_port(
        &self,
        preference: &RemotePortProbePreference,
        _local_connect_endpoint: &str,
    ) -> Result<RemotePortProbeResult, Self::Error> {
        let ssh_password = self.ssh_password()?;
        let mut command = if ssh_password.is_some() {
            let mut command = Command::new("sshpass");
            command.arg("-e").arg("ssh");
            command
        } else {
            Command::new("ssh")
        };
        if let Some(secret) = &ssh_password {
            command.env("SSHPASS", secret.expose_secret());
        }
        configure_ssh_command(&mut command);
        if let RemoteHostAuthProfile::Key { key_path } = &self.profile.auth {
            command.arg("-i").arg(key_path);
        }
        let output = command
            .arg(format!("{}@{}", self.profile.ssh_user, self.profile.host))
            .arg(remote_probe_command(preference))
            .stdin(Stdio::null())
            .output()
            .map_err(|error| RemotePortProbeError::new(error.to_string()))?;
        if !output.status.success() {
            return Err(RemotePortProbeError::new(format!(
                "remote port probe failed with status {}{}",
                output.status,
                stderr_summary(&output.stderr)
            )));
        }
        parse_probe_output(&String::from_utf8_lossy(&output.stdout))
    }
}

impl<S> SshRemotePortProbe<S>
where
    S: RemoteHostSecretStore,
    S::Error: ToString,
{
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

fn configure_ssh_command(command: &mut Command) {
    command
        .arg("-o")
        .arg("StrictHostKeyChecking=accept-new")
        .arg("-o")
        .arg("ConnectTimeout=10");
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
        use crate::runtime::remote_host::remote_host_secret_store::MemoryRemoteHostSecretStore;
        let profile = RemoteHostProfile {
            name: "130".to_string(),
            host: "10.1.29.130".to_string(),
            ssh_user: "kk".to_string(),
            auth: RemoteHostAuthProfile::Password {
                password_secret_id: None,
            },
            sudo_password_secret_id: None,
            preferred_remote_port:
                crate::runtime::remote_host::remote_host_history_store::RemotePortPreference::Auto,
            last_remote_port: None,
            last_endpoint: None,
            last_connected_at: None,
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
}
