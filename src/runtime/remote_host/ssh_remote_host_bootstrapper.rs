#![allow(dead_code)]

use crate::runtime::remote_host::remote_host_history_store::{
    RemoteHostAuthProfile, RemoteHostProfile,
};
use crate::runtime::remote_host::remote_host_secret_store::{
    FileRemoteHostSecretStore, RemoteHostSecretId, RemoteHostSecretStore, RemoteHostSecretValue,
};
use crate::runtime::remote_host::remote_ssh_executor::{
    RemoteSshAuth, RemoteSshExecutor, RemoteSshTarget, RusshRemoteSshExecutor,
};
use std::fmt;

pub const WAITAGENT_INSTALL_SCRIPT_URL: &str =
    "https://raw.githubusercontent.com/kikakkz/wait-agent/main/scripts/install.sh";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteWaitAgentStartPlan {
    pub remote_port: u16,
    pub local_connect_endpoint: String,
    pub authority_id: String,
    pub command: String,
}

impl RemoteWaitAgentStartPlan {
    pub fn new(
        remote_port: u16,
        local_connect_endpoint: impl Into<String>,
        authority_id: impl Into<String>,
    ) -> Self {
        let local_connect_endpoint = local_connect_endpoint.into();
        let authority_id = authority_id.into();
        Self {
            remote_port,
            command: format!(
                "nohup waitagent --port {remote_port} --connect {} --node-id {} __remote-daemon >/tmp/waitagent-{remote_port}.log 2>&1 < /dev/null &",
                shell_single_quote(&local_connect_endpoint),
                shell_single_quote(&authority_id)
            ),
            local_connect_endpoint,
            authority_id,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteHostBootstrapPlan {
    pub host: String,
    pub ssh_user: String,
    pub auth_kind: String,
    pub key_path: Option<String>,
    pub ssh_password_secret_id: Option<RemoteHostSecretId>,
    pub sudo_password_secret_id: Option<RemoteHostSecretId>,
    pub install_or_update_command: String,
    pub start_plan: RemoteWaitAgentStartPlan,
}

impl RemoteHostBootstrapPlan {
    pub fn from_profile(
        profile: &RemoteHostProfile,
        remote_port: u16,
        local_connect_endpoint: impl Into<String>,
        authority_id: impl Into<String>,
    ) -> Self {
        let (auth_kind, key_path, ssh_password_secret_id) = match &profile.auth {
            RemoteHostAuthProfile::Password { password_secret_id } => {
                ("password".to_string(), None, password_secret_id.clone())
            }
            RemoteHostAuthProfile::Key { key_path } => (
                "key".to_string(),
                Some(key_path.to_string_lossy().into_owned()),
                None,
            ),
        };
        let authority_id = authority_id.into();
        Self {
            host: profile.host.clone(),
            ssh_user: profile.ssh_user.clone(),
            auth_kind,
            key_path,
            ssh_password_secret_id,
            sudo_password_secret_id: profile.sudo_password_secret_id.clone(),
            install_or_update_command: install_or_update_command(),
            start_plan: RemoteWaitAgentStartPlan::new(
                remote_port,
                local_connect_endpoint,
                authority_id,
            ),
        }
    }
}

pub trait RemoteHostBootstrapper {
    type Error;

    fn ensure_waitagent_and_start(&self, plan: &RemoteHostBootstrapPlan)
        -> Result<(), Self::Error>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteHostBootstrapError {
    message: String,
}

impl RemoteHostBootstrapError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for RemoteHostBootstrapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for RemoteHostBootstrapError {}

#[derive(Debug, Clone)]
pub struct SshRemoteHostBootstrapper<S = FileRemoteHostSecretStore, E = RusshRemoteSshExecutor> {
    secret_store: S,
    ssh_executor: E,
}

impl Default for SshRemoteHostBootstrapper<FileRemoteHostSecretStore, RusshRemoteSshExecutor> {
    fn default() -> Self {
        Self {
            secret_store: FileRemoteHostSecretStore::default(),
            ssh_executor: RusshRemoteSshExecutor,
        }
    }
}

impl<S> SshRemoteHostBootstrapper<S, RusshRemoteSshExecutor> {
    pub fn new(secret_store: S) -> Self {
        Self {
            secret_store,
            ssh_executor: RusshRemoteSshExecutor,
        }
    }
}

impl<S, E> SshRemoteHostBootstrapper<S, E> {
    pub fn with_executor(secret_store: S, ssh_executor: E) -> Self {
        Self {
            secret_store,
            ssh_executor,
        }
    }
}

impl<S, E> RemoteHostBootstrapper for SshRemoteHostBootstrapper<S, E>
where
    S: RemoteHostSecretStore,
    S::Error: ToString,
    E: RemoteSshExecutor,
    E::Error: ToString,
{
    type Error = RemoteHostBootstrapError;

    fn ensure_waitagent_and_start(
        &self,
        plan: &RemoteHostBootstrapPlan,
    ) -> Result<(), Self::Error> {
        self.run_ssh_command(plan, &plan.install_or_update_command, true)?;
        self.run_ssh_command(plan, &plan.start_plan.command, false)
    }
}

impl<S, E> SshRemoteHostBootstrapper<S, E>
where
    S: RemoteHostSecretStore,
    S::Error: ToString,
    E: RemoteSshExecutor,
    E::Error: ToString,
{
    fn run_ssh_command(
        &self,
        plan: &RemoteHostBootstrapPlan,
        remote_command: &str,
        allow_sudo: bool,
    ) -> Result<(), RemoteHostBootstrapError> {
        let ssh_password = self.ssh_password(plan)?;
        let sudo_password = if allow_sudo {
            self.sudo_password(plan)?
        } else {
            None
        };
        let target = self.ssh_target(plan, ssh_password)?;
        let remote_command = if sudo_password.is_some() {
            format!("sudo -S sh -lc {}", shell_single_quote(remote_command))
        } else {
            remote_command.to_string()
        };
        let stdin = sudo_password
            .as_ref()
            .map(|secret| format!("{}\n", secret.expose_secret()));
        let output = self
            .ssh_executor
            .exec(&target, &remote_command, stdin.as_deref())
            .map_err(|error| RemoteHostBootstrapError::new(error.to_string()))?;
        if output.status == 0 {
            Ok(())
        } else {
            Err(RemoteHostBootstrapError::new(format!(
                "ssh remote bootstrap command failed with status {}{}",
                output.status,
                stderr_summary(&output.stderr)
            )))
        }
    }

    fn ssh_target(
        &self,
        plan: &RemoteHostBootstrapPlan,
        ssh_password: Option<RemoteHostSecretValue>,
    ) -> Result<RemoteSshTarget, RemoteHostBootstrapError> {
        let auth = match plan.auth_kind.as_str() {
            "password" => {
                let password = ssh_password.ok_or_else(|| {
                    RemoteHostBootstrapError::new("password auth requires a loaded SSH password")
                })?;
                RemoteSshAuth::Password {
                    password: password.expose_secret().to_string(),
                }
            }
            "key" => RemoteSshAuth::Key {
                key_path: plan
                    .key_path
                    .as_ref()
                    .map(std::path::PathBuf::from)
                    .ok_or_else(|| RemoteHostBootstrapError::new("key auth requires a key path"))?,
            },
            other => {
                return Err(RemoteHostBootstrapError::new(format!(
                    "unsupported remote host auth `{other}`"
                )))
            }
        };
        Ok(RemoteSshTarget {
            host: plan.host.clone(),
            port: 22,
            user: plan.ssh_user.clone(),
            auth,
        })
    }

    fn ssh_password(
        &self,
        plan: &RemoteHostBootstrapPlan,
    ) -> Result<Option<RemoteHostSecretValue>, RemoteHostBootstrapError> {
        if plan.auth_kind != "password" {
            return Ok(None);
        }
        let Some(secret_id) = &plan.ssh_password_secret_id else {
            return Err(RemoteHostBootstrapError::new(
                "password auth requires a saved SSH password secret id",
            ));
        };
        self.secret_store
            .get_secret(secret_id)
            .map_err(|error| RemoteHostBootstrapError::new(error.to_string()))?
            .ok_or_else(|| {
                RemoteHostBootstrapError::new(format!(
                    "SSH password secret `{}` was not found",
                    secret_id.as_str()
                ))
            })
            .map(Some)
    }

    fn sudo_password(
        &self,
        plan: &RemoteHostBootstrapPlan,
    ) -> Result<Option<RemoteHostSecretValue>, RemoteHostBootstrapError> {
        let Some(secret_id) = &plan.sudo_password_secret_id else {
            return Ok(None);
        };
        self.secret_store
            .get_secret(secret_id)
            .map_err(|error| RemoteHostBootstrapError::new(error.to_string()))?
            .ok_or_else(|| {
                RemoteHostBootstrapError::new(format!(
                    "sudo password secret `{}` was not found",
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

pub fn install_or_update_command() -> String {
    let expected_version = env!("CARGO_PKG_VERSION");
    let install = format!(
        "curl -fsSL {} | bash",
        shell_single_quote(WAITAGENT_INSTALL_SCRIPT_URL)
    );
    format!(
        "if ! command -v waitagent >/dev/null 2>&1 || ! waitagent --version 2>/dev/null | grep -q {}; then {}; fi",
        shell_single_quote(expected_version),
        install
    )
}
fn shell_single_quote(value: &str) -> String {
    let quote = char::from(39);
    let slash = char::from(92);
    let mut out = String::new();
    out.push(quote);
    for ch in value.chars() {
        if ch == quote {
            out.push(quote);
            out.push(slash);
            out.push(quote);
            out.push(quote);
        } else {
            out.push(ch);
        }
    }
    out.push(quote);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::remote_host::remote_host_history_store::{
        RemoteHostAuthProfile, RemoteHostProfile, RemotePortPreference,
    };
    use crate::runtime::remote_host::remote_ssh_executor::{
        RemoteSshExecutor, RemoteSshOutput, RemoteSshTarget,
    };
    use std::cell::RefCell;
    use std::rc::Rc;

    #[derive(Clone)]
    struct RecordingSshExecutor {
        calls: Rc<RefCell<Vec<(RemoteSshTarget, String, Option<String>)>>>,
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
            Ok(RemoteSshOutput {
                status: 0,
                stdout: Vec::new(),
                stderr: Vec::new(),
            })
        }
    }
    use crate::runtime::remote_host::remote_host_secret_store::{
        MemoryRemoteHostSecretStore, RemoteHostSecretStore,
    };

    #[test]
    fn remote_host_bootstrap_plan_uses_install_script_and_connects_back_to_local_server() {
        let profile = RemoteHostProfile {
            name: "130".to_string(),
            host: "10.1.29.130".to_string(),
            ssh_user: "kk".to_string(),
            auth: RemoteHostAuthProfile::Password {
                password_secret_id: None,
            },
            sudo_password_secret_id: None,
            preferred_remote_port: RemotePortPreference::Auto,
            last_remote_port: None,
            last_endpoint: None,
            last_connected_at: None,
            use_install_proxy: true,
        };

        let plan = RemoteHostBootstrapPlan::from_profile(
            &profile,
            7476,
            "10.1.26.84:7474",
            "10.1.29.130#7476",
        );

        assert!(plan
            .install_or_update_command
            .contains(WAITAGENT_INSTALL_SCRIPT_URL));
        assert!(plan
            .install_or_update_command
            .contains("command -v waitagent"));
        assert!(plan
            .install_or_update_command
            .contains("waitagent --version"));
        assert!(plan.install_or_update_command.contains("curl -fsSL"));
        assert!(plan.start_plan.command.contains(
            "waitagent --port 7476 --connect '10.1.26.84:7474' --node-id '10.1.29.130#7476' __remote-daemon"
        ));
        assert!(plan.start_plan.command.contains("nohup"));
    }

    #[test]
    fn remote_host_bootstrap_plan_carries_secret_ids_without_secret_values() {
        let ssh_id = RemoteHostSecretId::new("waitagent.remote-host.130.ssh-password").unwrap();
        let sudo_id = RemoteHostSecretId::new("waitagent.remote-host.130.sudo-password").unwrap();
        let store = MemoryRemoteHostSecretStore::default();
        store
            .put_secret(&ssh_id, RemoteHostSecretValue::new("ssh-secret"))
            .unwrap();
        store
            .put_secret(&sudo_id, RemoteHostSecretValue::new("sudo-secret"))
            .unwrap();
        let profile = RemoteHostProfile {
            name: "130".to_string(),
            host: "10.1.29.130".to_string(),
            ssh_user: "kk".to_string(),
            auth: RemoteHostAuthProfile::Password {
                password_secret_id: Some(ssh_id.clone()),
            },
            sudo_password_secret_id: Some(sudo_id.clone()),
            preferred_remote_port: RemotePortPreference::Auto,
            last_remote_port: None,
            last_endpoint: None,
            last_connected_at: None,
            use_install_proxy: true,
        };

        let plan = RemoteHostBootstrapPlan::from_profile(
            &profile,
            7476,
            "10.1.26.84:7474",
            "10.1.29.130#7476",
        );
        let bootstrapper = SshRemoteHostBootstrapper::new(store);

        assert_eq!(plan.ssh_password_secret_id, Some(ssh_id));
        assert_eq!(plan.sudo_password_secret_id, Some(sudo_id));
        assert!(!format!("{plan:?}").contains("ssh-secret"));
        assert!(!format!("{plan:?}").contains("sudo-secret"));
        assert_eq!(
            bootstrapper
                .ssh_password(&plan)
                .unwrap()
                .unwrap()
                .expose_secret(),
            "ssh-secret"
        );
        assert_eq!(
            bootstrapper
                .sudo_password(&plan)
                .unwrap()
                .unwrap()
                .expose_secret(),
            "sudo-secret"
        );
    }
    #[test]
    fn remote_host_bootstrapper_uses_in_process_ssh_executor() {
        let ssh_id = RemoteHostSecretId::new("waitagent.remote-host.130.ssh-password").unwrap();
        let sudo_id = RemoteHostSecretId::new("waitagent.remote-host.130.sudo-password").unwrap();
        let store = MemoryRemoteHostSecretStore::default();
        store
            .put_secret(&ssh_id, RemoteHostSecretValue::new("ssh-secret"))
            .unwrap();
        store
            .put_secret(&sudo_id, RemoteHostSecretValue::new("sudo-secret"))
            .unwrap();
        let profile = RemoteHostProfile {
            name: "130".to_string(),
            host: "10.1.29.130".to_string(),
            ssh_user: "kk".to_string(),
            auth: RemoteHostAuthProfile::Password {
                password_secret_id: Some(ssh_id),
            },
            sudo_password_secret_id: Some(sudo_id),
            preferred_remote_port: RemotePortPreference::Auto,
            last_remote_port: None,
            last_endpoint: None,
            last_connected_at: None,
            use_install_proxy: true,
        };
        let plan = RemoteHostBootstrapPlan::from_profile(
            &profile,
            7476,
            "10.1.26.84:7474",
            "10.1.29.130#7476",
        );
        let calls = Rc::new(RefCell::new(Vec::new()));
        let bootstrapper = SshRemoteHostBootstrapper::with_executor(
            store,
            RecordingSshExecutor {
                calls: calls.clone(),
            },
        );

        bootstrapper.ensure_waitagent_and_start(&plan).unwrap();

        let calls = calls.borrow();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].0.host, "10.1.29.130");
        assert_eq!(calls[0].0.user, "kk");
        assert!(calls[0].1.starts_with("sudo -S sh -lc "));
        assert_eq!(calls[0].2.as_deref(), Some("sudo-secret\n"));
        assert!(calls[1].1.contains("__remote-daemon"));
        assert_eq!(calls[1].2, None);
    }
}
