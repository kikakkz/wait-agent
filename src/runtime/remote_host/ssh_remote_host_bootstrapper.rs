#![allow(dead_code)]

use crate::runtime::remote_host::remote_host_history_store::{
    RemoteHostAuthProfile, RemoteHostProfile,
};
use crate::runtime::remote_host::remote_host_secret_store::{
    FileRemoteHostSecretStore, RemoteHostSecretId, RemoteHostSecretStore, RemoteHostSecretValue,
};
use crate::runtime::remote_host::remote_ssh_executor::{
    RemoteSshAuth, RemoteSshExecutor, RemoteSshOutput, RemoteSshTarget, RusshRemoteSshExecutor,
};
use std::fmt;

pub const WAITAGENT_INSTALL_SCRIPT_URL: &str =
    "https://raw.githubusercontent.com/kikakkz/wait-agent/main/scripts/install.sh";
const REMOTE_ENDPOINT_PREFLIGHT_TIMEOUT_SECS: u16 = 5;
const REMOTE_INSTALL_PREFLIGHT_TIMEOUT_SECS: u16 = 10;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteWaitAgentStartPlan {
    pub remote_port: u16,
    pub local_connect_endpoint: String,
    pub authority_id: String,
    pub endpoint_preflight_command: String,
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
            endpoint_preflight_command: endpoint_preflight_command(&local_connect_endpoint),
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
    pub install_reachability_preflight_command: Option<String>,
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
            install_reachability_preflight_command: None,
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
        self.run_ssh_command(
            plan,
            &plan.start_plan.endpoint_preflight_command,
            false,
        )
        .map_err(|error| {
            RemoteHostBootstrapError::new(format!(
                "remote host cannot reach local WaitAgent endpoint `{}`: {}. Pass `--public <host:port>` with an endpoint reachable from `{}`.",
                plan.start_plan.local_connect_endpoint, error, plan.host
            ))
        })?;
        if !self.remote_waitagent_is_current(plan)? {
            if let Some(command) = &plan.install_reachability_preflight_command {
                self.run_ssh_command(plan, command, false)
                    .map_err(|error| {
                        RemoteHostBootstrapError::new(format!(
                            "remote host cannot reach the WaitAgent install URL{}: {}",
                            install_proxy_hint(command),
                            error
                        ))
                    })?;
            }
            self.run_ssh_command(plan, &plan.install_or_update_command, true)?;
        }
        if self.remote_waitagent_daemon_is_running(plan)? {
            return Ok(());
        }
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
    fn remote_waitagent_is_current(
        &self,
        plan: &RemoteHostBootstrapPlan,
    ) -> Result<bool, RemoteHostBootstrapError> {
        let output = self.run_ssh_output(plan, &current_version_check_command(), false)?;
        Ok(output.status == 0)
    }

    fn remote_waitagent_daemon_is_running(
        &self,
        plan: &RemoteHostBootstrapPlan,
    ) -> Result<bool, RemoteHostBootstrapError> {
        let output = self.run_ssh_output(plan, &daemon_running_check_command(plan), false)?;
        Ok(output.status == 0)
    }

    fn run_ssh_command(
        &self,
        plan: &RemoteHostBootstrapPlan,
        remote_command: &str,
        allow_sudo: bool,
    ) -> Result<(), RemoteHostBootstrapError> {
        let output = self.run_ssh_output(plan, remote_command, allow_sudo)?;
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

    fn run_ssh_output(
        &self,
        plan: &RemoteHostBootstrapPlan,
        remote_command: &str,
        allow_sudo: bool,
    ) -> Result<RemoteSshOutput, RemoteHostBootstrapError> {
        let ssh_password = self.ssh_password(plan)?;
        let sudo_password = if allow_sudo {
            self.sudo_password(plan)?
        } else {
            None
        };
        let target = self.ssh_target(plan, ssh_password)?;
        let remote_command = if sudo_password.is_some() {
            sudo_shell_command(remote_command)
        } else {
            remote_command.to_string()
        };
        let stdin = sudo_password
            .as_ref()
            .map(|secret| format!("{}\n", secret.expose_secret()));
        self.ssh_executor
            .exec(&target, &remote_command, stdin.as_deref())
            .map_err(|error| RemoteHostBootstrapError::new(error.to_string()))
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
    let install = format!(
        "tmp=\"$(mktemp)\" && trap 'rm -f \"$tmp\"' EXIT && curl -fsSL --max-time 120 {} -o \"$tmp\" && bash \"$tmp\"",
        shell_single_quote(WAITAGENT_INSTALL_SCRIPT_URL)
    );
    format!(
        "if ! {{ {}; }}; then {}; fi",
        current_version_check_command(),
        install
    )
}

fn current_version_check_command() -> String {
    let expected_version = env!("CARGO_PKG_VERSION");
    format!(
        "command -v waitagent >/dev/null 2>&1 && waitagent --version 2>/dev/null | grep -q {}",
        shell_single_quote(expected_version)
    )
}

fn daemon_running_check_command(plan: &RemoteHostBootstrapPlan) -> String {
    format!(
        "ps -eo args= | grep -F -- {} | grep -F -- {} | grep -F -- {} | grep -F -- {} | grep -F -- {} | grep -v 'grep -F' >/dev/null 2>&1",
        shell_single_quote("waitagent"),
        shell_single_quote(&format!("--port {}", plan.start_plan.remote_port)),
        shell_single_quote(&format!("--connect {}", plan.start_plan.local_connect_endpoint)),
        shell_single_quote(&format!("--node-id {}", plan.start_plan.authority_id)),
        shell_single_quote("__remote-daemon"),
    )
}

pub fn install_reachability_preflight_command(env_prefixes: &[String]) -> String {
    let command = install_reachability_preflight_curl_command();
    let attempts = env_prefixes
        .iter()
        .map(|prefix| prefix.trim())
        .filter(|prefix| !prefix.is_empty())
        .map(|prefix| format!("{{ {prefix} {command}; }}"))
        .collect::<Vec<_>>();
    if !attempts.is_empty() {
        return attempts.join(" || ");
    }
    command
}

fn install_reachability_preflight_curl_command() -> String {
    format!(
        "curl -fsSL --connect-timeout {} --max-time {} -o /dev/null {}",
        REMOTE_ENDPOINT_PREFLIGHT_TIMEOUT_SECS,
        REMOTE_INSTALL_PREFLIGHT_TIMEOUT_SECS,
        shell_single_quote(WAITAGENT_INSTALL_SCRIPT_URL)
    )
}

fn install_proxy_hint(command: &str) -> &'static str {
    if command.contains("_proxy=") || command.contains("_PROXY=") {
        " through the configured install proxy"
    } else {
        ""
    }
}

fn endpoint_preflight_command(endpoint: &str) -> String {
    match parse_endpoint_host_port(endpoint) {
        Ok((host, port)) => tcp_connect_preflight_command(&host, port),
        Err(message) => format!("echo {} >&2; exit 2", shell_single_quote(&message)),
    }
}

fn tcp_connect_preflight_command(host: &str, port: u16) -> String {
    let host = shell_single_quote(host);
    let port = shell_single_quote(&port.to_string());
    let python = shell_single_quote(
        "import socket,sys; s=socket.create_connection((sys.argv[1], int(sys.argv[2])), 5); s.close()",
    );
    let bash = shell_single_quote("cat < /dev/null > /dev/tcp/$1/$2");
    format!(
        "if command -v nc >/dev/null 2>&1; then nc -z -w {timeout} {host} {port}; \
elif command -v python3 >/dev/null 2>&1; then python3 -c {python} {host} {port}; \
elif command -v bash >/dev/null 2>&1 && command -v timeout >/dev/null 2>&1; then timeout {timeout} bash -c {bash} sh {host} {port}; \
else echo 'no TCP probe tool available on remote host (need nc, python3, or bash+timeout)' >&2; exit 127; fi",
        timeout = REMOTE_ENDPOINT_PREFLIGHT_TIMEOUT_SECS
    )
}

fn parse_endpoint_host_port(endpoint: &str) -> Result<(String, u16), String> {
    let value = endpoint.trim();
    if value.is_empty() {
        return Err("local WaitAgent endpoint is empty".to_string());
    }
    let value = value
        .strip_prefix("http://")
        .or_else(|| value.strip_prefix("https://"))
        .unwrap_or(value);
    let value = value.split('/').next().unwrap_or(value);
    if let Some(rest) = value.strip_prefix('[') {
        let Some((host, tail)) = rest.split_once(']') else {
            return Err(format!(
                "local WaitAgent endpoint `{endpoint}` has an invalid IPv6 host"
            ));
        };
        let Some(port) = tail.strip_prefix(':') else {
            return Err(format!(
                "local WaitAgent endpoint `{endpoint}` is missing a port"
            ));
        };
        return parse_endpoint_port(endpoint, host, port);
    }
    let Some((host, port)) = value.rsplit_once(':') else {
        return Err(format!(
            "local WaitAgent endpoint `{endpoint}` is missing a port"
        ));
    };
    parse_endpoint_port(endpoint, host, port)
}

fn parse_endpoint_port(endpoint: &str, host: &str, port: &str) -> Result<(String, u16), String> {
    if host.trim().is_empty() {
        return Err(format!(
            "local WaitAgent endpoint `{endpoint}` is missing a host"
        ));
    }
    let port = port
        .parse::<u16>()
        .map_err(|_| format!("local WaitAgent endpoint `{endpoint}` has an invalid port"))?;
    Ok((host.to_string(), port))
}

fn sudo_shell_command(remote_command: &str) -> String {
    format!(
        "sudo -S -p '' sh -lc {}",
        shell_single_quote(remote_command)
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
        statuses: Rc<RefCell<Vec<u32>>>,
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
            let status = self.statuses.borrow_mut().pop().unwrap_or(0);
            Ok(RemoteSshOutput {
                status,
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
        assert!(plan.install_or_update_command.contains("if ! { command -v"));
        assert!(plan.install_or_update_command.contains("; }; then"));
        assert!(plan
            .install_or_update_command
            .contains("waitagent --version"));
        assert!(plan.install_or_update_command.contains("curl -fsSL"));
        assert!(plan
            .start_plan
            .endpoint_preflight_command
            .contains("10.1.26.84"));
        assert!(plan
            .start_plan
            .endpoint_preflight_command
            .contains("'7474'"));
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
                statuses: Rc::new(RefCell::new(vec![0, 1, 0, 1, 0])),
            },
        );

        bootstrapper.ensure_waitagent_and_start(&plan).unwrap();

        let calls = calls.borrow();
        assert_eq!(calls.len(), 5);
        assert_eq!(calls[0].0.host, "10.1.29.130");
        assert_eq!(calls[0].0.user, "kk");
        assert!(calls[0].1.contains("nc -z -w"));
        assert_eq!(calls[0].2, None);
        assert!(calls[1].1.contains("waitagent --version"));
        assert_eq!(calls[1].2, None);
        assert!(calls[2].1.starts_with("sudo -S -p '' sh -lc "));
        assert_eq!(calls[2].2.as_deref(), Some("sudo-secret\n"));
        assert!(calls[3].1.contains("ps -eo args="));
        assert_eq!(calls[3].2, None);
        assert!(calls[4].1.contains("__remote-daemon"));
        assert_eq!(calls[4].2, None);
    }

    #[test]
    fn remote_host_bootstrapper_checks_install_url_before_install_when_configured() {
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
        let mut plan = RemoteHostBootstrapPlan::from_profile(
            &profile,
            7476,
            "10.1.26.84:7474",
            "10.1.29.130#7476",
        );
        let env_prefixes = vec![
            "all_proxy='socks5://127.0.0.1:7897'".to_string(),
            "https_proxy='http://127.0.0.1:7897'".to_string(),
        ];
        plan.install_reachability_preflight_command =
            Some(install_reachability_preflight_command(&env_prefixes));
        let calls = Rc::new(RefCell::new(Vec::new()));
        let bootstrapper = SshRemoteHostBootstrapper::with_executor(
            store,
            RecordingSshExecutor {
                calls: calls.clone(),
                statuses: Rc::new(RefCell::new(vec![0, 1, 0, 0, 1, 0])),
            },
        );

        bootstrapper.ensure_waitagent_and_start(&plan).unwrap();

        let calls = calls.borrow();
        assert_eq!(calls.len(), 6);
        assert!(calls[0].1.contains("nc -z -w"));
        assert!(calls[1].1.contains("waitagent --version"));
        assert!(calls[2].1.contains("all_proxy="));
        assert!(calls[2].1.contains("https_proxy="));
        assert!(calls[2].1.contains(" || "));
        assert!(calls[2].1.contains(WAITAGENT_INSTALL_SCRIPT_URL));
        assert!(calls[3].1.starts_with("sudo -S -p '' sh -lc "));
        assert!(calls[4].1.contains("ps -eo args="));
        assert!(calls[5].1.contains("__remote-daemon"));
    }

    #[test]
    fn remote_host_bootstrapper_reports_unreachable_local_endpoint_before_starting() {
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
            preferred_remote_port: RemotePortPreference::Auto,
            last_remote_port: None,
            last_endpoint: None,
            last_connected_at: None,
            use_install_proxy: true,
        };
        let plan = RemoteHostBootstrapPlan::from_profile(
            &profile,
            7476,
            "192.168.31.178:7474",
            "10.1.29.130#7476",
        );
        let calls = Rc::new(RefCell::new(Vec::new()));
        let bootstrapper = SshRemoteHostBootstrapper::with_executor(
            store,
            RecordingSshExecutor {
                calls: calls.clone(),
                statuses: Rc::new(RefCell::new(vec![1])),
            },
        );

        let error = bootstrapper.ensure_waitagent_and_start(&plan).unwrap_err();

        assert!(error
            .to_string()
            .contains("remote host cannot reach local WaitAgent endpoint"));
        assert!(error.to_string().contains("--public <host:port>"));
        assert_eq!(calls.borrow().len(), 1);
    }

    #[test]
    fn endpoint_preflight_command_rejects_malformed_endpoint() {
        let command = endpoint_preflight_command("127.0.0.1");

        assert!(command.contains("missing a port"));
        assert!(command.contains("exit 2"));
    }

    #[test]
    fn remote_host_bootstrapper_skips_sudo_install_when_waitagent_is_current() {
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
                statuses: Rc::new(RefCell::new(vec![0, 1, 0, 0])),
            },
        );

        bootstrapper.ensure_waitagent_and_start(&plan).unwrap();

        let calls = calls.borrow();
        assert_eq!(calls.len(), 4);
        assert!(calls[0].1.contains("nc -z -w"));
        assert_eq!(calls[0].2, None);
        assert!(calls[1].1.contains("waitagent --version"));
        assert_eq!(calls[1].2, None);
        assert!(calls[2].1.contains("ps -eo args="));
        assert_eq!(calls[2].2, None);
        assert!(calls[3].1.contains("__remote-daemon"));
        assert_eq!(calls[3].2, None);
        assert!(!calls.iter().any(|(_, command, _)| command.contains("sudo")));
    }

    #[test]
    fn remote_host_bootstrapper_does_not_start_when_daemon_is_running() {
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
                statuses: Rc::new(RefCell::new(vec![0, 0, 0])),
            },
        );

        bootstrapper.ensure_waitagent_and_start(&plan).unwrap();

        let calls = calls.borrow();
        assert_eq!(calls.len(), 3);
        assert!(calls[0].1.contains("nc -z -w"));
        assert!(calls[1].1.contains("waitagent --version"));
        assert!(calls[2].1.contains("ps -eo args="));
        assert!(!calls
            .iter()
            .any(|(_, command, _)| command.contains("nohup")));
    }
}
