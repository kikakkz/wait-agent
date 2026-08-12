use crate::host::ssh::remote_host_history_store::{RemoteHostAuthProfile, RemoteHostProfile};
use crate::host::ssh::remote_host_secret_store::{
    FileRemoteHostSecretStore, RemoteHostSecretId, RemoteHostSecretStore, RemoteHostSecretValue,
};
use crate::host::ssh::remote_ssh_executor::{
    RemoteSshAuth, RemoteSshExecutor, RemoteSshOutput, RemoteSshTarget, RusshRemoteSshExecutor,
};
use crate::infra::node_credentials::NodeCredentialPaths;
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
    pub credential_paths: NodeCredentialPaths,
    pub endpoint_preflight_command: String,
    pub credentials_command: String,
    pub command: String,
    pub subcommand: String,
    /// When true, the remote daemon listens for an outbound dial from the
    /// control host instead of connecting back with `--connect`.
    pub outbound_dial: bool,
}

impl RemoteWaitAgentStartPlan {
    pub fn new(
        remote_port: u16,
        local_connect_endpoint: impl Into<String>,
        authority_id: impl Into<String>,
    ) -> Self {
        Self::new_with_mode(
            remote_port,
            local_connect_endpoint,
            authority_id,
            true, /* outbound_dial */
        )
    }

    pub fn new_with_mode(
        remote_port: u16,
        local_connect_endpoint: impl Into<String>,
        authority_id: impl Into<String>,
        outbound_dial: bool,
    ) -> Self {
        let local_connect_endpoint = local_connect_endpoint.into();
        let authority_id = authority_id.into();
        let credential_paths = NodeCredentialPaths::remote_default_paths();
        let endpoint_preflight_command = if outbound_dial {
            String::new()
        } else {
            endpoint_preflight_command(&local_connect_endpoint)
        };
        let command = if outbound_dial {
            format!(
                "nohup waitagent --port {remote_port} --node-id {} --node-key-path {} --node-cert-path {} __ratatui-node-server >/tmp/waitagent-{remote_port}.log 2>&1 < /dev/null &",
                shell_single_quote(&authority_id),
                remote_shell_path(&credential_paths.key_path),
                remote_shell_path(&credential_paths.cert_path)
            )
        } else {
            format!(
                "nohup waitagent --port {remote_port} --connect {} --node-id {} --node-key-path {} --node-cert-path {} __ratatui-node-server >/tmp/waitagent-{remote_port}.log 2>&1 < /dev/null &",
                shell_single_quote(&local_connect_endpoint),
                shell_single_quote(&authority_id),
                remote_shell_path(&credential_paths.key_path),
                remote_shell_path(&credential_paths.cert_path)
            )
        };
        Self {
            remote_port,
            credential_paths: credential_paths.clone(),
            endpoint_preflight_command,
            credentials_command: generate_credentials_command(remote_port, &credential_paths),
            command,
            local_connect_endpoint,
            authority_id,
            subcommand: "__ratatui-node-server".to_string(),
            outbound_dial,
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
    /// When set, the bootstrapper deploys the local waitagent binary to the
    /// remote host via this script instead of running the curl-based installer.
    pub deploy_script_path: Option<String>,
    /// Remote path where the deployed binary is installed. Used by the deploy
    /// script and for version/daemon checks.
    pub remote_bin_path: String,
    /// OpenSSH-formatted operator public key to install on the remote host.
    pub operator_public_key: Option<String>,
}

// TODO(cleanup): transitional remote code, kept for Phase 8 wiring.
#[allow(dead_code)]
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
        let start_plan =
            RemoteWaitAgentStartPlan::new(remote_port, local_connect_endpoint, authority_id);
        let remote_bin_path = "$HOME/.local/bin/waitagent".to_string();
        Self {
            host: profile.host.clone(),
            ssh_user: profile.ssh_user.clone(),
            auth_kind,
            key_path,
            ssh_password_secret_id,
            sudo_password_secret_id: profile.sudo_password_secret_id.clone(),
            install_or_update_command: install_or_update_command(),
            install_reachability_preflight_command: None,
            start_plan,
            deploy_script_path: None,
            remote_bin_path,
            operator_public_key: None,
        }
    }

    /// Configure this plan to deploy the locally-built waitagent binary to the
    /// remote host using the repository deployment script instead of the curl
    /// release installer. The script copies target/release/waitagent to the
    /// remote host, kills any existing daemon on the same port, and starts a
    /// fresh ratatui node server in the background with nohup.
    pub fn with_local_binary_deploy(mut self) -> Self {
        self.deploy_script_path = Some(default_deploy_script_path());
        self.install_or_update_command = deploy_command(&self);
        self.start_plan.subcommand = "__ratatui-node-server".to_string();
        self.start_plan.command = if self.start_plan.outbound_dial {
            format!(
                "nohup waitagent --port {} --node-id {} --node-key-path {} --node-cert-path {} {} >/tmp/waitagent-{}.log 2>&1 < /dev/null &",
                self.start_plan.remote_port,
                shell_single_quote(&self.start_plan.authority_id),
                remote_shell_path(&self.start_plan.credential_paths.key_path),
                remote_shell_path(&self.start_plan.credential_paths.cert_path),
                shell_single_quote(&self.start_plan.subcommand),
                self.start_plan.remote_port,
            )
        } else {
            format!(
                "nohup waitagent --port {} --connect {} --node-id {} --node-key-path {} --node-cert-path {} {} >/tmp/waitagent-{}.log 2>&1 < /dev/null &",
                self.start_plan.remote_port,
                shell_single_quote(&self.start_plan.local_connect_endpoint),
                shell_single_quote(&self.start_plan.authority_id),
                remote_shell_path(&self.start_plan.credential_paths.key_path),
                remote_shell_path(&self.start_plan.credential_paths.cert_path),
                shell_single_quote(&self.start_plan.subcommand),
                self.start_plan.remote_port,
            )
        };
        self
    }
}

pub trait RemoteHostBootstrapper {
    type Error;

    fn ensure_waitagent_and_start(
        &self,
        plan: &RemoteHostBootstrapPlan,
    ) -> Result<RemoteHostBootstrapResult, Self::Error>;
}

/// Result returned by a successful remote host bootstrap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteHostBootstrapResult {
    pub tls_pin_sha256: String,
    pub remote_port: u16,
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

// TODO(cleanup): transitional remote code, kept for Phase 8 wiring.
#[allow(dead_code)]
impl<S> SshRemoteHostBootstrapper<S, RusshRemoteSshExecutor> {
    pub fn new(secret_store: S) -> Self {
        Self {
            secret_store,
            ssh_executor: RusshRemoteSshExecutor,
        }
    }
}

// TODO(cleanup): transitional remote code, kept for Phase 8 wiring.
#[allow(dead_code)]
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
    ) -> Result<RemoteHostBootstrapResult, Self::Error> {
        if !plan.start_plan.outbound_dial && !plan.start_plan.endpoint_preflight_command.is_empty()
        {
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
        }

        if plan.deploy_script_path.is_some() {
            self.run_deploy_script(plan)?;
            let (tls_pin_sha256, remote_port) = self.generate_credentials_and_parse(plan)?;
            if !self.remote_waitagent_daemon_is_running(plan)? {
                self.run_ssh_command(plan, &plan.start_plan.command, false)?;
            }
            return Ok(RemoteHostBootstrapResult {
                tls_pin_sha256,
                remote_port,
            });
        }

        self.run_ssh_command(plan, &ensure_waitagent_home_command(), false)?;

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

        let (tls_pin_sha256, remote_port) = self.generate_credentials_and_parse(plan)?;

        if let Some(public_key) = &plan.operator_public_key {
            self.install_operator_public_key(plan, public_key)?;
        }

        if !self.remote_waitagent_daemon_is_running(plan)? {
            self.run_ssh_command(plan, &plan.start_plan.command, false)?;
        }

        Ok(RemoteHostBootstrapResult {
            tls_pin_sha256,
            remote_port,
        })
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

    pub fn remote_waitagent_daemon_is_running(
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

    fn generate_credentials_and_parse(
        &self,
        plan: &RemoteHostBootstrapPlan,
    ) -> Result<(String, u16), RemoteHostBootstrapError> {
        let output = self.run_ssh_output(plan, &plan.start_plan.credentials_command, false)?;
        if output.status != 0 {
            return Err(RemoteHostBootstrapError::new(format!(
                "remote credential generation failed with status {}{}",
                output.status,
                stderr_summary(&output.stderr)
            )));
        }
        let (tls_pin_sha256, remote_port) = parse_credentials_output(&output.stdout)
            .ok_or_else(|| {
                let stdout = String::from_utf8_lossy(&output.stdout);
                RemoteHostBootstrapError::new(format!(
                    "remote credential generation did not emit WAITAGENT_CREDENTIALS marker; stdout: {stdout}"
                ))
            })?;
        Ok((tls_pin_sha256, remote_port))
    }

    fn install_operator_public_key(
        &self,
        plan: &RemoteHostBootstrapPlan,
        public_key: &str,
    ) -> Result<(), RemoteHostBootstrapError> {
        let dir = "$HOME/.waitagent/authorized_operators";
        let fingerprint = operator_public_key_fingerprint(public_key)
            .map_err(|error| RemoteHostBootstrapError::new(error.to_string()))?;
        let path = format!("{dir}/{fingerprint}.pub");
        let command = format!(
            "mkdir -p {dir} && printf '%s\\n' {} > {path}",
            shell_single_quote(public_key)
        );
        self.run_ssh_command(plan, &command, true)?;
        Ok(())
    }

    fn run_deploy_script(
        &self,
        plan: &RemoteHostBootstrapPlan,
    ) -> Result<(), RemoteHostBootstrapError> {
        let Some(script_path) = &plan.deploy_script_path else {
            return Err(RemoteHostBootstrapError::new(
                "deploy script path is not configured",
            ));
        };
        let ssh_password = self.ssh_password(plan)?;
        let mut command = std::process::Command::new(script_path);
        command
            .arg("--host")
            .arg(&plan.host)
            .arg("--user")
            .arg(&plan.ssh_user)
            .arg("--remote-port")
            .arg(plan.start_plan.remote_port.to_string())
            .arg("--connect")
            .arg(&plan.start_plan.local_connect_endpoint)
            .arg("--node-id")
            .arg(&plan.start_plan.authority_id)
            .arg("--remote-bin")
            .arg(&plan.remote_bin_path);
        if let Some(key_path) = &plan.key_path {
            command.arg("--identity").arg(key_path);
        }
        if let Some(password) = ssh_password {
            command.env("WAITAGENT_SSH_PASSWORD", password.expose_secret());
        }
        let output = command.output().map_err(|error| {
            RemoteHostBootstrapError::new(format!("deploy script failed: {error}"))
        })?;
        if output.status.success() {
            Ok(())
        } else {
            let _stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            Err(RemoteHostBootstrapError::new(format!(
                "deploy script exited with status {}{}",
                output.status,
                stderr_summary(stderr.as_bytes())
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

// TODO(cleanup): transitional remote code, kept for Phase 8 wiring.
#[allow(dead_code)]
fn default_deploy_script_path() -> String {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    format!("{}/scripts/deploy-ratatui-remote.sh", manifest_dir)
}

// TODO(cleanup): transitional remote code, kept for Phase 8 wiring.
#[allow(dead_code)]
fn deploy_command(plan: &RemoteHostBootstrapPlan) -> String {
    let mut parts = vec![
        shell_single_quote(plan.deploy_script_path.as_deref().unwrap_or("")),
        "--host".to_string(),
        shell_single_quote(&plan.host),
        "--user".to_string(),
        shell_single_quote(&plan.ssh_user),
        "--remote-port".to_string(),
        shell_single_quote(&plan.start_plan.remote_port.to_string()),
    ];
    if !plan.start_plan.outbound_dial {
        parts.push("--connect".to_string());
        parts.push(shell_single_quote(&plan.start_plan.local_connect_endpoint));
    }
    parts.push("--node-id".to_string());
    parts.push(shell_single_quote(&plan.start_plan.authority_id));
    parts.push("--node-key-path".to_string());
    parts.push(shell_single_quote(
        &plan.start_plan.credential_paths.key_path.to_string_lossy(),
    ));
    parts.push("--node-cert-path".to_string());
    parts.push(shell_single_quote(
        &plan.start_plan.credential_paths.cert_path.to_string_lossy(),
    ));
    parts.push("--remote-bin".to_string());
    parts.push(shell_single_quote(&plan.remote_bin_path));
    if let Some(key_path) = &plan.key_path {
        parts.push("--identity".to_string());
        parts.push(shell_single_quote(key_path));
    }
    parts.join(" ")
}

fn current_version_check_command() -> String {
    let expected_version = env!("CARGO_PKG_VERSION");
    format!(
        "command -v waitagent >/dev/null 2>&1 && waitagent --version 2>/dev/null | grep -q {}",
        shell_single_quote(expected_version)
    )
}

fn ensure_waitagent_home_command() -> String {
    "mkdir -p $HOME/.waitagent".to_string()
}

fn generate_credentials_command(
    remote_port: u16,
    credential_paths: &NodeCredentialPaths,
) -> String {
    format!(
        "waitagent --port {} --node-key-path {} --node-cert-path {} __generate-node-credentials",
        shell_single_quote(&remote_port.to_string()),
        remote_shell_path(&credential_paths.key_path),
        remote_shell_path(&credential_paths.cert_path),
    )
}

fn parse_credentials_output(stdout: &[u8]) -> Option<(String, u16)> {
    let text = String::from_utf8_lossy(stdout);
    for line in text.lines() {
        let line = line.trim();
        let Some(payload) = line.strip_prefix("WAITAGENT_CREDENTIALS") else {
            continue;
        };
        let Some((fingerprint, port)) = payload.rsplit_once(':') else {
            continue;
        };
        let fingerprint = fingerprint.trim();
        let port = port.trim().parse::<u16>().ok()?;
        if !fingerprint.is_empty() {
            return Some((fingerprint.to_string(), port));
        }
    }
    None
}

fn operator_public_key_fingerprint(public_key: &str) -> Result<String, String> {
    let public_key = ssh_key::PublicKey::from_openssh(public_key)
        .map_err(|error| format!("failed to parse operator public key: {error}"))?;
    Ok(crate::infra::operator_auth::public_key_fingerprint(
        &public_key,
    ))
}

fn daemon_running_check_command(plan: &RemoteHostBootstrapPlan) -> String {
    if plan.start_plan.outbound_dial {
        format!(
            "ps -eo args= | grep -F -- {} | grep -F -- {} | grep -F -- {} | grep -F -- {} | grep -v 'grep -F' >/dev/null 2>&1",
            shell_single_quote("waitagent"),
            shell_single_quote(&format!("--port {}", plan.start_plan.remote_port)),
            shell_single_quote(&format!("--node-id {}", plan.start_plan.authority_id)),
            shell_single_quote(&plan.start_plan.subcommand),
        )
    } else {
        format!(
            "ps -eo args= | grep -F -- {} | grep -F -- {} | grep -F -- {} | grep -F -- {} | grep -F -- {} | grep -v 'grep -F' >/dev/null 2>&1",
            shell_single_quote("waitagent"),
            shell_single_quote(&format!("--port {}", plan.start_plan.remote_port)),
            shell_single_quote(&format!("--connect {}", plan.start_plan.local_connect_endpoint)),
            shell_single_quote(&format!("--node-id {}", plan.start_plan.authority_id)),
            shell_single_quote(&plan.start_plan.subcommand),
        )
    }
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

/// Quote a path for embedding in a remote shell command.
///
/// Paths that start with `$HOME/` are left in double quotes so the remote
/// shell expands the variable; local-style paths are single-quoted as usual.
fn remote_shell_path(path: &std::path::Path) -> String {
    let s = path.to_string_lossy();
    if s.starts_with("$HOME/") {
        format!("\"{s}\"")
    } else {
        shell_single_quote(&s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::ssh::remote_host_history_store::{
        RemoteHostAuthProfile, RemoteHostProfile, RemotePortPreference,
    };
    use crate::host::ssh::remote_ssh_executor::{
        RemoteSshExecutor, RemoteSshOutput, RemoteSshTarget,
    };
    use std::cell::RefCell;
    use std::rc::Rc;

    type SshCallLog = Vec<(RemoteSshTarget, String, Option<String>)>;

    #[derive(Clone)]
    struct RecordingSshExecutor {
        calls: Rc<RefCell<SshCallLog>>,
        statuses: Rc<RefCell<Vec<u32>>>,
        credentials_stdout: Rc<RefCell<Option<String>>>,
    }

    impl RecordingSshExecutor {
        #[allow(dead_code)]
        fn with_credentials_stdout(self, stdout: impl Into<String>) -> Self {
            *self.credentials_stdout.borrow_mut() = Some(stdout.into());
            self
        }
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
            let stdout = if command.contains("__generate-node-credentials") {
                self.credentials_stdout
                    .borrow_mut()
                    .take()
                    .unwrap_or_default()
                    .into_bytes()
            } else {
                Vec::new()
            };
            Ok(RemoteSshOutput {
                status,
                stdout,
                stderr: Vec::new(),
            })
        }
    }
    use crate::host::ssh::remote_host_secret_store::{
        MemoryRemoteHostSecretStore, RemoteHostSecretStore,
    };

    #[test]
    fn remote_host_bootstrap_plan_uses_install_script_and_outbound_dial() {
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
            tls_pin_sha256: None,
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
        assert!(plan.start_plan.endpoint_preflight_command.is_empty());
        assert!(!plan.start_plan.command.contains("--connect"));
        assert!(plan
            .start_plan
            .command
            .contains("--node-id '10.1.29.130#7476'"));
        assert!(plan
            .start_plan
            .command
            .contains("waitagent --port 7476 --node-id"));
        assert!(plan.start_plan.command.contains("--node-key-path"));
        assert!(plan.start_plan.command.contains("--node-cert-path"));
        assert!(plan.start_plan.command.contains("__ratatui-node-server"));
        assert!(plan.start_plan.command.contains("nohup"));
        assert!(plan.start_plan.outbound_dial);
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
            tls_pin_sha256: None,
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
            tls_pin_sha256: None,
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
                statuses: Rc::new(RefCell::new(vec![0, 1, 0, 0, 1, 0])),
                credentials_stdout: Rc::new(RefCell::new(Some(
                    "WAITAGENT_CREDENTIALSdeadbeef:7476\n".to_string(),
                ))),
            },
        );

        let result = bootstrapper.ensure_waitagent_and_start(&plan).unwrap();
        assert_eq!(result.tls_pin_sha256, "deadbeef");
        assert_eq!(result.remote_port, 7476);

        let calls = calls.borrow();
        assert_eq!(calls.len(), 6);
        assert_eq!(calls[0].0.host, "10.1.29.130");
        assert_eq!(calls[0].0.user, "kk");
        assert!(calls[0].1.contains("mkdir -p"));
        assert_eq!(calls[0].2, None);
        assert!(calls[1].1.contains("waitagent --version"));
        assert_eq!(calls[1].2, None);
        assert!(calls[2].1.starts_with("sudo -S -p '' sh -lc "));
        assert_eq!(calls[2].2.as_deref(), Some("sudo-secret\n"));
        assert!(calls[3].1.contains("__generate-node-credentials"));
        assert_eq!(calls[3].2, None);
        assert!(calls[4].1.contains("ps -eo args="));
        assert_eq!(calls[4].2, None);
        assert!(calls[5].1.contains("__ratatui-node-server"));
        assert_eq!(calls[5].2, None);
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
            tls_pin_sha256: None,
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
                statuses: Rc::new(RefCell::new(vec![0, 1, 0, 0, 0, 1, 0])),
                credentials_stdout: Rc::new(RefCell::new(Some(
                    "WAITAGENT_CREDENTIALSdeadbeef:7476\n".to_string(),
                ))),
            },
        );

        bootstrapper.ensure_waitagent_and_start(&plan).unwrap();

        let calls = calls.borrow();
        assert_eq!(calls.len(), 7);
        assert!(calls[0].1.contains("mkdir -p"));
        assert!(calls[1].1.contains("waitagent --version"));
        assert!(calls[2].1.contains("all_proxy="));
        assert!(calls[2].1.contains("https_proxy="));
        assert!(calls[2].1.contains(" || "));
        assert!(calls[2].1.contains(WAITAGENT_INSTALL_SCRIPT_URL));
        assert!(calls[3].1.starts_with("sudo -S -p '' sh -lc "));
        assert!(calls[4].1.contains("__generate-node-credentials"));
        assert!(calls[5].1.contains("ps -eo args="));
        assert!(calls[6].1.contains("__ratatui-node-server"));
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
            tls_pin_sha256: None,
        };
        let mut plan = RemoteHostBootstrapPlan::from_profile(
            &profile,
            7476,
            "192.168.31.178:7474",
            "10.1.29.130#7476",
        );
        // Force inbound mode so the local-endpoint preflight is exercised.
        plan.start_plan = RemoteWaitAgentStartPlan::new_with_mode(
            plan.start_plan.remote_port,
            plan.start_plan.local_connect_endpoint.clone(),
            plan.start_plan.authority_id.clone(),
            false,
        );
        let calls = Rc::new(RefCell::new(Vec::new()));
        let bootstrapper = SshRemoteHostBootstrapper::with_executor(
            store,
            RecordingSshExecutor {
                calls: calls.clone(),
                statuses: Rc::new(RefCell::new(vec![1])),
                credentials_stdout: Rc::new(RefCell::new(None)),
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
            tls_pin_sha256: None,
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
                statuses: Rc::new(RefCell::new(vec![0, 1, 0, 0, 0])),
                credentials_stdout: Rc::new(RefCell::new(Some(
                    "WAITAGENT_CREDENTIALSdeadbeef:7476\n".to_string(),
                ))),
            },
        );

        bootstrapper.ensure_waitagent_and_start(&plan).unwrap();

        let calls = calls.borrow();
        assert_eq!(calls.len(), 5);
        assert!(calls[0].1.contains("mkdir -p"));
        assert_eq!(calls[0].2, None);
        assert!(calls[1].1.contains("waitagent --version"));
        assert_eq!(calls[1].2, None);
        assert!(calls[2].1.contains("__generate-node-credentials"));
        assert_eq!(calls[2].2, None);
        assert!(calls[3].1.contains("ps -eo args="));
        assert_eq!(calls[3].2, None);
        assert!(calls[4].1.contains("__ratatui-node-server"));
        assert_eq!(calls[4].2, None);
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
            tls_pin_sha256: None,
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
                statuses: Rc::new(RefCell::new(vec![0, 0, 0, 0])),
                credentials_stdout: Rc::new(RefCell::new(Some(
                    "WAITAGENT_CREDENTIALSdeadbeef:7476\n".to_string(),
                ))),
            },
        );

        bootstrapper.ensure_waitagent_and_start(&plan).unwrap();

        let calls = calls.borrow();
        assert_eq!(calls.len(), 4);
        assert!(calls[0].1.contains("mkdir -p"));
        assert!(calls[1].1.contains("waitagent --version"));
        assert!(calls[2].1.contains("__generate-node-credentials"));
        assert!(calls[3].1.contains("ps -eo args="));
        assert!(!calls
            .iter()
            .any(|(_, command, _)| command.contains("nohup")));
    }

    #[test]
    fn remote_host_bootstrap_plan_with_local_deploy_uses_repo_script_and_outbound_args() {
        let profile = RemoteHostProfile {
            name: "130".to_string(),
            host: "10.1.29.130".to_string(),
            ssh_user: "kk".to_string(),
            auth: RemoteHostAuthProfile::Key {
                key_path: std::path::PathBuf::from("/home/kk/.ssh/id_rsa"),
            },
            sudo_password_secret_id: None,
            preferred_remote_port: RemotePortPreference::Auto,
            last_remote_port: None,
            last_endpoint: None,
            last_connected_at: None,
            use_install_proxy: true,
            tls_pin_sha256: None,
        };

        let plan = RemoteHostBootstrapPlan::from_profile(
            &profile,
            7476,
            "10.1.26.84:7474",
            "10.1.29.130#7476",
        )
        .with_local_binary_deploy();

        assert!(plan.deploy_script_path.is_some());
        assert!(plan
            .deploy_script_path
            .as_deref()
            .unwrap()
            .contains("scripts/deploy-ratatui-remote.sh"));
        let command = plan.install_or_update_command;
        assert!(command.contains("deploy-ratatui-remote.sh"));
        assert!(command.contains("--host"));
        assert!(command.contains("10.1.29.130"));
        assert!(command.contains("--user"));
        assert!(command.contains("kk"));
        assert!(command.contains("--remote-port"));
        assert!(command.contains("7476"));
        assert!(!command.contains("--connect"));
        assert!(command.contains("--node-id"));
        assert!(command.contains("10.1.29.130#7476"));
        assert!(command.contains("--node-key-path"));
        assert!(command.contains("--node-cert-path"));
        assert!(command.contains("--identity"));
        assert!(command.contains("/home/kk/.ssh/id_rsa"));
        assert!(command.contains("--remote-bin"));
        assert!(command.contains("$HOME/.local/bin/waitagent"));
        assert!(plan.start_plan.command.contains("__ratatui-node-server"));
        assert!(!plan.start_plan.command.contains("--connect"));
        assert!(plan.start_plan.command.contains("--node-id"));
    }
}
