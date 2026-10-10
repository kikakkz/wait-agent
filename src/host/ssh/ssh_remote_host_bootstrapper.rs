use crate::host::ssh::local_artifact_cache::{
    ArtifactTarget, LocalArtifactCache, UreqArtifactFetcher,
};
use crate::host::ssh::remote_host_history_store::{
    InstallSource, RemoteHostAuthProfile, RemoteHostProfile,
};
use crate::host::ssh::remote_host_secret_store::{
    DefaultRemoteHostSecretStore, KeyringRemoteHostSecretStore, RemoteHostSecretId,
    RemoteHostSecretStore, RemoteHostSecretValue,
};
use crate::host::ssh::remote_shell::RemoteShellKind;
use crate::host::ssh::remote_ssh_executor::{
    RemoteSshAuth, RemoteSshExecutor, RemoteSshOutput, RemoteSshTarget, RusshRemoteSshExecutor,
};
use crate::infra::node_credentials::NodeCredentialPaths;
use std::fmt;

pub const WAITAGENT_INSTALL_SCRIPT_URL: &str =
    "https://raw.githubusercontent.com/kikakkz/wait-agent/main/scripts/install.sh";
/// GitHub release download base for the Windows zip archive
/// (`waitagent-<version>-x86_64-windows.zip`, matching
/// `scripts/install.ps1`).
const WAITAGENT_WINDOWS_RELEASE_BASE_URL: &str =
    "https://github.com/kikakkz/wait-agent/releases/download";
/// PowerShell expression resolving to the per-user install location on a
/// Windows target (`%LOCALAPPDATA%\Programs\waitagent\waitagent.exe`, matching
/// the local `scripts/install.ps1` installer).
const WINDOWS_WAITAGENT_EXE_PS: &str =
    "Join-Path $env:LOCALAPPDATA 'Programs\\waitagent\\waitagent.exe'";
const WINDOWS_WAITAGENT_HOME_PS: &str = "Join-Path $env:USERPROFILE '.waitagent'";
const WINDOWS_NODE_KEY_PS: &str = "$env:USERPROFILE\\.waitagent\\node.key";
const WINDOWS_NODE_CERT_PS: &str = "$env:USERPROFILE\\.waitagent\\node.crt";
const WINDOWS_AUTHORIZED_OPERATORS_PS: &str = "$env:USERPROFILE\\.waitagent\\authorized_operators";
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
        remote_shell: RemoteShellKind,
    ) -> Self {
        Self::new_with_mode(
            remote_port,
            local_connect_endpoint,
            authority_id,
            true, /* outbound_dial */
            remote_shell,
        )
    }

    pub fn new_with_mode(
        remote_port: u16,
        local_connect_endpoint: impl Into<String>,
        authority_id: impl Into<String>,
        outbound_dial: bool,
        remote_shell: RemoteShellKind,
    ) -> Self {
        let local_connect_endpoint = local_connect_endpoint.into();
        let authority_id = authority_id.into();
        let credential_paths = NodeCredentialPaths::remote_default_paths();
        let endpoint_preflight_command = if outbound_dial {
            String::new()
        } else {
            endpoint_preflight_command(&local_connect_endpoint, remote_shell)
        };
        let command = match remote_shell {
            RemoteShellKind::Posix => {
                if outbound_dial {
                    format!(
                        "nohup waitagent --port {remote_port} --node-id {} --node-key-path {} --node-cert-path {} __ratatui-node-server >/tmp/waitagent-{remote_port}.log 2>&1 < /dev/null & {}",
                        shell_single_quote(&authority_id),
                        remote_shell_path(&credential_paths.key_path),
                        remote_shell_path(&credential_paths.cert_path),
                        wait_for_port_ready_shell(remote_port)
                    )
                } else {
                    format!(
                        "nohup waitagent --port {remote_port} --connect {} --node-id {} --node-key-path {} --node-cert-path {} __ratatui-node-server >/tmp/waitagent-{remote_port}.log 2>&1 < /dev/null & {}",
                        shell_single_quote(&local_connect_endpoint),
                        shell_single_quote(&authority_id),
                        remote_shell_path(&credential_paths.key_path),
                        remote_shell_path(&credential_paths.cert_path),
                        wait_for_port_ready_shell(remote_port)
                    )
                }
            }
            RemoteShellKind::Windows => windows_start_command(
                remote_port,
                &local_connect_endpoint,
                &authority_id,
                outbound_dial,
            ),
        };
        Self {
            remote_port,
            credential_paths: credential_paths.clone(),
            endpoint_preflight_command,
            credentials_command: generate_credentials_command(
                remote_port,
                &credential_paths,
                remote_shell,
            ),
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
    /// Port the remote `sshd` listens on (defaults to 22 when unset).
    pub ssh_port: u16,
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
    /// Where the install artifact comes from (issue #168). `LocalUpload`
    /// downloads the release artifact on this machine (reusing the local
    /// cache) and uploads it to the remote host over the SSH exec channel;
    /// POSIX targets only — Windows targets error out explicitly.
    pub install_source: InstallSource,
    /// Detected shell family of the remote host. Every remote command
    /// generator (`install_or_update_command`, `start_plan`, version check,
    /// daemon check, ...) emits a POSIX or PowerShell script based on this
    /// kind (`docs/windows-ssh-target-design.md` tasks T2/T3).
    pub remote_shell: RemoteShellKind,
}

// TODO(cleanup): transitional remote code, kept for Phase 8 wiring.
#[allow(dead_code)]
impl RemoteHostBootstrapPlan {
    pub fn from_profile(
        profile: &RemoteHostProfile,
        remote_port: u16,
        local_connect_endpoint: impl Into<String>,
        authority_id: impl Into<String>,
        remote_shell: RemoteShellKind,
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
        let start_plan = RemoteWaitAgentStartPlan::new(
            remote_port,
            local_connect_endpoint,
            authority_id,
            remote_shell,
        );
        let remote_bin_path = "$HOME/.local/bin/waitagent".to_string();
        Self {
            host: profile.host.clone(),
            ssh_port: profile.ssh_port(),
            ssh_user: profile.ssh_user.clone(),
            auth_kind,
            key_path,
            ssh_password_secret_id,
            sudo_password_secret_id: profile.sudo_password_secret_id.clone(),
            install_or_update_command: install_or_update_command_for(remote_shell),
            install_reachability_preflight_command: None,
            start_plan,
            deploy_script_path: None,
            remote_bin_path,
            operator_public_key: None,
            install_source: profile.install_source,
            remote_shell,
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
                "nohup waitagent --port {} --node-id {} --node-key-path {} --node-cert-path {} {} >/tmp/waitagent-{}.log 2>&1 < /dev/null & {}",
                self.start_plan.remote_port,
                shell_single_quote(&self.start_plan.authority_id),
                remote_shell_path(&self.start_plan.credential_paths.key_path),
                remote_shell_path(&self.start_plan.credential_paths.cert_path),
                shell_single_quote(&self.start_plan.subcommand),
                self.start_plan.remote_port,
                wait_for_port_ready_shell(self.start_plan.remote_port),
            )
        } else {
            format!(
                "nohup waitagent --port {} --connect {} --node-id {} --node-key-path {} --node-cert-path {} {} >/tmp/waitagent-{}.log 2>&1 < /dev/null & {}",
                self.start_plan.remote_port,
                shell_single_quote(&self.start_plan.local_connect_endpoint),
                shell_single_quote(&self.start_plan.authority_id),
                remote_shell_path(&self.start_plan.credential_paths.key_path),
                remote_shell_path(&self.start_plan.credential_paths.cert_path),
                shell_single_quote(&self.start_plan.subcommand),
                self.start_plan.remote_port,
                wait_for_port_ready_shell(self.start_plan.remote_port),
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
pub struct SshRemoteHostBootstrapper<S = DefaultRemoteHostSecretStore, E = RusshRemoteSshExecutor> {
    secret_store: S,
    ssh_executor: E,
    /// Local cache of verified release artifacts for the `LocalUpload`
    /// install source (issue #168). Production resolves the real
    /// `~/.waitagent/cache/`; tests inject a temp dir via
    /// [`Self::with_artifact_cache`].
    artifact_cache: LocalArtifactCache,
}

impl Default for SshRemoteHostBootstrapper<DefaultRemoteHostSecretStore, RusshRemoteSshExecutor> {
    fn default() -> Self {
        Self {
            secret_store: KeyringRemoteHostSecretStore,
            ssh_executor: RusshRemoteSshExecutor,
            artifact_cache: LocalArtifactCache::default(),
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
            artifact_cache: LocalArtifactCache::default(),
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
            artifact_cache: LocalArtifactCache::default(),
        }
    }

    /// Scope the local artifact cache to a custom directory. Tests point it
    /// at a temp dir primed with a fixture artifact so the upload path can
    /// run without network access; production relies on `Default`, which
    /// resolves the real `~/.waitagent/cache/`.
    pub fn with_artifact_cache(mut self, artifact_cache: LocalArtifactCache) -> Self {
        self.artifact_cache = artifact_cache;
        self
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

        if plan.install_source == InstallSource::LocalUpload {
            return self.ensure_waitagent_and_start_via_upload(plan);
        }

        if plan.deploy_script_path.is_some() {
            if plan.remote_shell == RemoteShellKind::Windows {
                return Err(RemoteHostBootstrapError::new(
                    "local binary deploy is not supported for windows remote hosts",
                ));
            }
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

        self.run_ssh_command(
            plan,
            &ensure_waitagent_home_command(plan.remote_shell),
            false,
        )?;

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
            // sudo only exists for the POSIX installer (root install into
            // /usr/local/bin); the Windows installer is per-user and needs no
            // elevation (`docs/windows-ssh-target-design.md` §6.5).
            let allow_sudo = plan.remote_shell == RemoteShellKind::Posix;
            self.run_ssh_command(plan, &plan.install_or_update_command, allow_sudo)?;
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
        let output = self.run_ssh_output(
            plan,
            &current_version_check_command(plan.remote_shell),
            false,
        )?;
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
        self.run_ssh_command_with_stdin(plan, remote_command, None, allow_sudo)
    }

    /// Like [`Self::run_ssh_command`] but with an explicit stdin payload
    /// (the upload path streams base64 chunk data this way). A sudo password
    /// always wins the stdin slot: sudo-wrapped commands are exactly the
    /// ones that need the password prompt answered, and the upload chunks
    /// never run sudo-wrapped.
    fn run_ssh_command_with_stdin(
        &self,
        plan: &RemoteHostBootstrapPlan,
        remote_command: &str,
        stdin_payload: Option<&str>,
        allow_sudo: bool,
    ) -> Result<(), RemoteHostBootstrapError> {
        let output =
            self.run_ssh_output_with_stdin(plan, remote_command, stdin_payload, allow_sudo)?;
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
        self.run_ssh_output_with_stdin(plan, remote_command, None, allow_sudo)
    }

    fn run_ssh_output_with_stdin(
        &self,
        plan: &RemoteHostBootstrapPlan,
        remote_command: &str,
        stdin_payload: Option<&str>,
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
        let stdin = match (&sudo_password, stdin_payload) {
            (Some(secret), _) => Some(format!("{}\n", secret.expose_secret())),
            (None, payload) => payload.map(str::to_string),
        };
        self.ssh_executor
            .exec(&target, &remote_command, stdin.as_deref())
            .map_err(|error| RemoteHostBootstrapError::new(error.to_string()))
    }

    fn generate_credentials_and_parse(
        &self,
        plan: &RemoteHostBootstrapPlan,
    ) -> Result<(String, u16), RemoteHostBootstrapError> {
        self.generate_credentials_output(plan, &plan.start_plan.credentials_command)
    }

    /// Upload install source (issue #168): this machine provides the
    /// artifact and the remote host installs it. The remote needs no
    /// outbound access — the download happens locally (reusing the
    /// `~/.waitagent/cache/` artifact when a verified copy exists) and the
    /// bytes travel over the SSH exec channel as base64 chunks — so the
    /// install-URL reachability preflight and the remote-install proxy
    /// wrapping do not apply to this path and are skipped by design.
    fn ensure_waitagent_and_start_via_upload(
        &self,
        plan: &RemoteHostBootstrapPlan,
    ) -> Result<RemoteHostBootstrapResult, RemoteHostBootstrapError> {
        if plan.remote_shell == RemoteShellKind::Windows {
            return Err(RemoteHostBootstrapError::new(
                "install source Upload is not supported for Windows remote hosts: the local upload path only supports POSIX targets; switch Install Source to Remote for this host",
            ));
        }
        self.run_ssh_command(
            plan,
            &ensure_waitagent_home_command(plan.remote_shell),
            false,
        )?;
        // One exec per connect detects the remote target triple and uid
        // before any artifact selection; the arch is deliberately not
        // persisted in the profile because reprovisioning can change it.
        let detection = self.detect_upload_target(plan)?;
        let install_dir = if detection.uid == 0 || plan.sudo_password_secret_id.is_some() {
            "/usr/local/bin"
        } else {
            // install.sh parity: no root and no sudo password means the
            // per-user location (the layout the deploy script already uses).
            "$HOME/.local/bin"
        };
        let bin_path = format!("{install_dir}/waitagent");
        if !self.upload_waitagent_is_current(plan, &bin_path)? {
            let artifact = self
                .artifact_cache
                .ensure_artifact(
                    detection.target,
                    env!("CARGO_PKG_VERSION"),
                    &UreqArtifactFetcher,
                )
                .map_err(|error| {
                    RemoteHostBootstrapError::new(format!(
                        "failed to prepare the local waitagent artifact for upload: {error}"
                    ))
                })?;
            let data = std::fs::read(&artifact).map_err(|error| {
                RemoteHostBootstrapError::new(format!(
                    "failed to read cached artifact {}: {error}",
                    artifact.display()
                ))
            })?;
            self.upload_artifact(plan, &data)?;
            // sudo only wraps the decode+install step, and only when the
            // remote user is not root and a sudo password is configured.
            let allow_sudo = detection.uid != 0 && plan.sudo_password_secret_id.is_some();
            self.run_ssh_command(
                plan,
                &upload_install_command(plan, detection.target, install_dir, &bin_path),
                allow_sudo,
            )?;
        }
        // The credentials/start commands reference the explicit install path
        // instead of relying on the remote PATH (the $HOME fallback dir is
        // not necessarily on it for non-interactive exec sessions).
        let credentials_command = upload_credentials_command(plan, &bin_path);
        let (tls_pin_sha256, remote_port) =
            self.generate_credentials_output(plan, &credentials_command)?;
        if let Some(public_key) = &plan.operator_public_key {
            self.install_operator_public_key(plan, public_key)?;
        }
        let start_command = upload_start_command(plan, &bin_path);
        if !self.remote_waitagent_daemon_is_running(plan)? {
            self.run_ssh_command(plan, &start_command, false)?;
        }
        Ok(RemoteHostBootstrapResult {
            tls_pin_sha256,
            remote_port,
        })
    }

    /// Runs the single arch/uid detection exec and parses its three lines
    /// (`uname -s`, `uname -m`, `id -u`). Cached per connect by the caller.
    fn detect_upload_target(
        &self,
        plan: &RemoteHostBootstrapPlan,
    ) -> Result<UploadTargetDetection, RemoteHostBootstrapError> {
        let output = self.run_ssh_output(plan, "uname -s && uname -m && id -u", false)?;
        if output.status != 0 {
            return Err(RemoteHostBootstrapError::new(format!(
                "remote arch detection failed with status {}{}",
                output.status,
                stderr_summary(&output.stderr)
            )));
        }
        parse_upload_detection(&output.stdout)
    }

    /// Version gate against the explicit upload install location (the
    /// generic `command -v waitagent` check cannot see the `$HOME` fallback
    /// directory on a non-interactive remote PATH).
    fn upload_waitagent_is_current(
        &self,
        plan: &RemoteHostBootstrapPlan,
        bin_path: &str,
    ) -> Result<bool, RemoteHostBootstrapError> {
        let command = format!(
            "{} --version 2>/dev/null | grep -q {}",
            remote_shell_path(std::path::Path::new(bin_path)),
            shell_single_quote(env!("CARGO_PKG_VERSION"))
        );
        Ok(self.run_ssh_output(plan, &command, false)?.status == 0)
    }

    /// Streams the artifact to a fixed-name remote temp file as base64
    /// chunks, one SSH exec per chunk (the executor opens exec channels
    /// only; there is no SFTP dependency). The payload rides the exec's
    /// stdin — sshd caps exec command lines far below one chunk's size.
    fn upload_artifact(
        &self,
        plan: &RemoteHostBootstrapPlan,
        data: &[u8],
    ) -> Result<(), RemoteHostBootstrapError> {
        for (command, payload) in upload_chunks(plan.start_plan.remote_port, data) {
            self.run_ssh_command_with_stdin(plan, &command, Some(&payload), false)?;
        }
        Ok(())
    }

    fn generate_credentials_output(
        &self,
        plan: &RemoteHostBootstrapPlan,
        credentials_command: &str,
    ) -> Result<(String, u16), RemoteHostBootstrapError> {
        let output = self.run_ssh_output(plan, credentials_command, false)?;
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
        let fingerprint = operator_public_key_fingerprint(public_key)
            .map_err(|error| RemoteHostBootstrapError::new(error.to_string()))?;
        if plan.remote_shell == RemoteShellKind::Windows {
            let command = windows_install_operator_public_key_command(public_key, &fingerprint);
            self.run_ssh_command(plan, &command, false)?;
            return Ok(());
        }
        let dir = "$HOME/.waitagent/authorized_operators";
        let path = format!("{dir}/{fingerprint}.pub");
        let command = format!(
            "mkdir -p {dir} && printf '%s\\n' {} > {path}",
            shell_single_quote(public_key)
        );
        // Never sudo here: the directory lives under the target user's
        // `$HOME`, and a sudo-wrapped command would expand `$HOME` to root's
        // home, silently installing the key where the per-user node server
        // never looks.
        self.run_ssh_command(plan, &command, false)?;
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
            port: plan.ssh_port,
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
        current_version_check_command(RemoteShellKind::Posix),
        install
    )
}

/// Install command for the remote host's shell family: POSIX keeps the
/// install.sh pipeline byte-for-byte, Windows downloads the release zip with
/// curl.exe and unpacks it with tar.exe (task T3).
pub fn install_or_update_command_for(remote_shell: RemoteShellKind) -> String {
    match remote_shell {
        RemoteShellKind::Posix => install_or_update_command(),
        RemoteShellKind::Windows => windows_install_or_update_command(None, None),
    }
}

/// Binary bytes per base64 upload chunk: 768 KiB encodes to exactly 1 MiB of
/// base64 text — a comfortable single SSH exec command line, and few enough
/// execs for a ~10 MB artifact (issue #168).
const UPLOAD_CHUNK_BYTES: usize = 768 * 1024;

/// Remote arch/uid detection result for one connect (issue #168). Deliberately
/// not persisted: reprovisioning can change the remote arch under the same
/// profile name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct UploadTargetDetection {
    target: ArtifactTarget,
    uid: u32,
}

/// Parses the three-line output of `uname -s && uname -m && id -u` into a
/// target triple and uid. An unsupported pair produces a typed error naming
/// what was detected and what the upload path supports.
fn parse_upload_detection(
    stdout: &[u8],
) -> Result<UploadTargetDetection, RemoteHostBootstrapError> {
    let text = String::from_utf8_lossy(stdout);
    let mut lines = text.lines();
    let system = lines.next().unwrap_or_default().trim();
    let machine = lines.next().unwrap_or_default().trim();
    let uid_line = lines.next().unwrap_or_default().trim();
    let uid = uid_line.parse::<u32>().map_err(|_| {
        RemoteHostBootstrapError::new(format!(
            "remote arch detection returned an unparsable uid `{uid_line}`; stdout: {text}"
        ))
    })?;
    let target = ArtifactTarget::from_uname(system, machine)
        .map_err(|error| RemoteHostBootstrapError::new(error.to_string()))?;
    Ok(UploadTargetDetection { target, uid })
}

fn upload_b64_path(remote_port: u16) -> String {
    format!("/tmp/.waitagent-upload-{remote_port}.b64")
}

fn upload_tarball_path(remote_port: u16) -> String {
    format!("/tmp/.waitagent-upload-{remote_port}.tar.gz")
}

/// One base64 append exec per chunk: the command is a tiny `cat > tmp` /
/// `cat >> tmp`, and the base64 payload travels as the exec's stdin —
/// channel data is packetized to the negotiated size, while a megabyte-long
/// command line exceeds sshd's exec-request limit (`Bad packet length`).
/// The first chunk's `>` truncates any stale temp file from a failed
/// attempt. Kept side-effect free so the chunking and command construction
/// are unit-testable without SSH.
fn upload_chunks(remote_port: u16, data: &[u8]) -> Vec<(String, String)> {
    let b64_path = upload_b64_path(remote_port);
    data.chunks(UPLOAD_CHUNK_BYTES)
        .enumerate()
        .map(|(index, chunk)| {
            let redirect = if index == 0 { ">" } else { ">>" };
            let command = format!("cat {redirect} {}", shell_single_quote(&b64_path));
            let payload = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, chunk);
            (command, payload)
        })
        .collect()
}

/// One-shot decode + install pipeline, run after the last chunk lands. Keeps
/// install.sh parity: unpack the release tarball, atomic chmod-755 install
/// into the install dir, `setcap cap_net_admin+ep` on Linux where available
/// (never fatal, matching install.sh), then verify the installed binary
/// reports the expected version before the exec reports success. Temp files
/// are removed regardless of the outcome.
fn upload_install_command(
    plan: &RemoteHostBootstrapPlan,
    target: ArtifactTarget,
    install_dir: &str,
    bin_path: &str,
) -> String {
    let remote_port = plan.start_plan.remote_port;
    let b64_path = upload_b64_path(remote_port);
    let tarball_path = upload_tarball_path(remote_port);
    let bin = remote_shell_path(std::path::Path::new(bin_path));
    let version = shell_single_quote(env!("CARGO_PKG_VERSION"));
    let setcap = if target == ArtifactTarget::LinuxX86_64 {
        format!(
            "(command -v setcap >/dev/null 2>&1 && setcap cap_net_admin+ep {bin} 2>/dev/null || true) && "
        )
    } else {
        String::new()
    };
    format!(
        "tmpd=\"$(mktemp -d)\" && {{ base64 -d {b64_path} > {tarball_path} \
&& tar xzf {tarball_path} -C \"$tmpd\" \
&& test -f \"$tmpd/waitagent\" \
&& mkdir -p \"{install_dir}\" \
&& cp \"$tmpd/waitagent\" \"{install_dir}/.waitagent.tmp\" \
&& chmod 755 \"{install_dir}/.waitagent.tmp\" \
&& mv -f \"{install_dir}/.waitagent.tmp\" {bin} \
&& {setcap}rm -rf \"$tmpd\" {b64_path} {tarball_path} \
&& {bin} --version 2>/dev/null | grep -q {version}; }}; \
rc=$?; rm -rf \"$tmpd\" >/dev/null 2>&1; exit $rc"
    )
}

/// Credentials command for the upload path: identical to the POSIX flow but
/// referencing the explicit install location (`$HOME` paths stay
/// double-quoted so the remote shell expands them).
fn upload_credentials_command(plan: &RemoteHostBootstrapPlan, bin_path: &str) -> String {
    format!(
        "{} --port {} --node-key-path {} --node-cert-path {} __generate-node-credentials",
        remote_shell_path(std::path::Path::new(bin_path)),
        shell_single_quote(&plan.start_plan.remote_port.to_string()),
        remote_shell_path(&plan.start_plan.credential_paths.key_path),
        remote_shell_path(&plan.start_plan.credential_paths.cert_path),
    )
}

/// Start command for the upload path: the POSIX nohup form with the explicit
/// install location instead of the bare `waitagent` PATH lookup.
fn upload_start_command(plan: &RemoteHostBootstrapPlan, bin_path: &str) -> String {
    let bin = remote_shell_path(std::path::Path::new(bin_path));
    let remote_port = plan.start_plan.remote_port;
    if plan.start_plan.outbound_dial {
        format!(
            "nohup {bin} --port {remote_port} --node-id {} --node-key-path {} --node-cert-path {} __ratatui-node-server >/tmp/waitagent-{remote_port}.log 2>&1 < /dev/null & {}",
            shell_single_quote(&plan.start_plan.authority_id),
            remote_shell_path(&plan.start_plan.credential_paths.key_path),
            remote_shell_path(&plan.start_plan.credential_paths.cert_path),
            wait_for_port_ready_shell(remote_port),
        )
    } else {
        format!(
            "nohup {bin} --port {remote_port} --connect {} --node-id {} --node-key-path {} --node-cert-path {} __ratatui-node-server >/tmp/waitagent-{remote_port}.log 2>&1 < /dev/null & {}",
            shell_single_quote(&plan.start_plan.local_connect_endpoint),
            shell_single_quote(&plan.start_plan.authority_id),
            remote_shell_path(&plan.start_plan.credential_paths.key_path),
            remote_shell_path(&plan.start_plan.credential_paths.cert_path),
            wait_for_port_ready_shell(remote_port),
        )
    }
}

// TODO(cleanup): transitional remote code, kept for Phase 8 wiring.
#[allow(dead_code)]
fn default_deploy_script_path() -> String {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    format!("{manifest_dir}/scripts/deploy-ratatui-remote.sh")
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

fn current_version_check_command(remote_shell: RemoteShellKind) -> String {
    match remote_shell {
        RemoteShellKind::Posix => {
            let expected_version = env!("CARGO_PKG_VERSION");
            format!(
                "command -v waitagent >/dev/null 2>&1 && waitagent --version 2>/dev/null | grep -q {}",
                shell_single_quote(expected_version)
            )
        }
        RemoteShellKind::Windows => windows_current_version_check_command(),
    }
}

fn windows_current_version_check_command() -> String {
    let expected_version = env!("CARGO_PKG_VERSION");
    let script = format!(
        "$exe = {WINDOWS_WAITAGENT_EXE_PS}; \
if (-not (Test-Path $exe)) {{ exit 1 }}; \
$out = & $exe --version 2>$null; \
if ($LASTEXITCODE -ne 0) {{ exit 1 }}; \
if (\"$out\" -like {}) {{ exit 0 }} else {{ exit 1 }}",
        ps_single_quote(&format!("*{expected_version}*"))
    );
    powershell_command(&script)
}

fn ensure_waitagent_home_command(remote_shell: RemoteShellKind) -> String {
    match remote_shell {
        RemoteShellKind::Posix => "mkdir -p $HOME/.waitagent".to_string(),
        RemoteShellKind::Windows => {
            let script = format!(
                "$dir = {WINDOWS_WAITAGENT_HOME_PS}; \
New-Item -ItemType Directory -Force $dir | Out-Null; \
if (Test-Path $dir) {{ exit 0 }} else {{ exit 1 }}"
            );
            powershell_command(&script)
        }
    }
}

fn generate_credentials_command(
    remote_port: u16,
    credential_paths: &NodeCredentialPaths,
    remote_shell: RemoteShellKind,
) -> String {
    match remote_shell {
        RemoteShellKind::Posix => format!(
            "waitagent --port {} --node-key-path {} --node-cert-path {} __generate-node-credentials",
            shell_single_quote(&remote_port.to_string()),
            remote_shell_path(&credential_paths.key_path),
            remote_shell_path(&credential_paths.cert_path),
        ),
        RemoteShellKind::Windows => {
            // The credentials subcommand is cross-platform; only the binary
            // and key/cert paths take Windows form
            // (`docs/windows-ssh-target-design.md` §2).
            let script = format!(
                "$exe = {WINDOWS_WAITAGENT_EXE_PS}; \
& $exe --port {remote_port} --node-key-path \"{WINDOWS_NODE_KEY_PS}\" --node-cert-path \"{WINDOWS_NODE_CERT_PS}\" __generate-node-credentials; \
exit $LASTEXITCODE"
            );
            powershell_command(&script)
        }
    }
}

/// Windows start command: a plain foreground exec of the installed binary.
/// Over an SSH exec session the server self-daemonizes
/// (`daemonize_self_if_needed`, task T4) and the parent copy exits once the
/// port accepts connections, so — unlike the POSIX `nohup … &` form — this
/// carries no shell-level backgrounding and returns when the daemon is ready.
fn windows_start_command(
    remote_port: u16,
    local_connect_endpoint: &str,
    authority_id: &str,
    outbound_dial: bool,
) -> String {
    let mut args = format!(
        "--port {remote_port} --node-id {} ",
        ps_single_quote(authority_id)
    );
    if !outbound_dial {
        args.push_str(&format!(
            "--connect {} ",
            ps_single_quote(local_connect_endpoint)
        ));
    }
    let script = format!(
        "$exe = {WINDOWS_WAITAGENT_EXE_PS}; \
& $exe {args}--node-key-path \"{WINDOWS_NODE_KEY_PS}\" --node-cert-path \"{WINDOWS_NODE_CERT_PS}\" __ratatui-node-server; \
exit $LASTEXITCODE"
    );
    powershell_command(&script)
}

/// Shell snippet that waits up to ~10 seconds for the remote waitagent to open
/// its listening port. Embedded into the daemon start command so the SSH
/// session does not return until the daemon is actually reachable, eliminating
/// the race where the control host dials before the remote process is ready.
fn wait_for_port_ready_shell(remote_port: u16) -> String {
    format!(
        "i=0; while [ $i -lt 50 ]; do if bash -c 'exec 3<>/dev/tcp/127.0.0.1/{remote_port}' >/dev/null 2>&1; then break; fi; sleep 0.2; i=$((i+1)); done; if [ $i -eq 50 ]; then exit 1; fi"
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
    if plan.remote_shell == RemoteShellKind::Windows {
        return windows_daemon_running_check_command(plan);
    }
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

fn windows_daemon_running_check_command(plan: &RemoteHostBootstrapPlan) -> String {
    let mut needles = vec![
        format!("--port {}", plan.start_plan.remote_port),
        format!("--node-id {}", plan.start_plan.authority_id),
        plan.start_plan.subcommand.clone(),
    ];
    if !plan.start_plan.outbound_dial {
        needles.push(format!(
            "--connect {}",
            plan.start_plan.local_connect_endpoint
        ));
    }
    let conditions = needles
        .iter()
        .map(|needle| {
            format!(
                "($_.CommandLine -like {})",
                ps_single_quote(&format!("*{needle}*"))
            )
        })
        .collect::<Vec<_>>()
        .join(" -and ");
    let script = format!(
        "$procs = @(Get-CimInstance Win32_Process -Filter \"Name='waitagent.exe'\" | Where-Object {{ {conditions} }}); \
if ($procs.Count -gt 0) {{ exit 0 }} else {{ exit 1 }}"
    );
    powershell_command(&script)
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

/// Windows install preflight: `curl.exe` must exist and be able to reach the
/// release zip (HEAD request only — the zip is multi-MB and full downloads
/// time out through slow proxies). Proxies are injected as `$env:`
/// assignments inside the script, per `docs/windows-ssh-target-design.md` §2.
pub fn windows_install_reachability_preflight_command(
    all_proxy: Option<&str>,
    https_proxy: Option<&str>,
) -> String {
    let url = windows_release_zip_url(env!("CARGO_PKG_VERSION"));
    let mut script = missing_curl_or_tar_guard();
    push_proxy_assignments(&mut script, all_proxy, https_proxy);
    script.push_str(&format!(
        "curl.exe -fsSIL --connect-timeout {REMOTE_ENDPOINT_PREFLIGHT_TIMEOUT_SECS} --max-time {REMOTE_INSTALL_PREFLIGHT_TIMEOUT_SECS} -o NUL {}; exit $LASTEXITCODE",
        ps_single_quote(&url)
    ));
    powershell_command(&script)
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

fn endpoint_preflight_command(endpoint: &str, remote_shell: RemoteShellKind) -> String {
    match parse_endpoint_host_port(endpoint) {
        Ok((host, port)) => match remote_shell {
            RemoteShellKind::Posix => tcp_connect_preflight_command(&host, port),
            RemoteShellKind::Windows => windows_tcp_connect_preflight_command(&host, port),
        },
        Err(message) => match remote_shell {
            RemoteShellKind::Posix => {
                format!("echo {} >&2; exit 2", shell_single_quote(&message))
            }
            RemoteShellKind::Windows => powershell_command(&format!(
                "Write-Error {}; exit 2",
                ps_single_quote(&message)
            )),
        },
    }
}

fn windows_tcp_connect_preflight_command(host: &str, port: u16) -> String {
    let script = format!(
        "$client = New-Object Net.Sockets.TcpClient; \
$result = $client.BeginConnect({}, {port}, $null, $null); \
$completed = $result.AsyncWaitHandle.WaitOne({}, $false); \
if (-not $completed) {{ exit 1 }}; \
try {{ $client.EndConnect($result); exit 0 }} catch {{ exit 1 }}",
        ps_single_quote(host),
        REMOTE_ENDPOINT_PREFLIGHT_TIMEOUT_SECS * 1000,
    );
    powershell_command(&script)
}

fn tcp_connect_preflight_command(host: &str, port: u16) -> String {
    let host = shell_single_quote(host);
    let port = shell_single_quote(&port.to_string());
    let python = shell_single_quote(
        "import socket,sys; s=socket.create_connection((sys.argv[1], int(sys.argv[2])), 5); s.close()",
    );
    let bash = shell_single_quote("cat < /dev/null > /dev/tcp/$1/$2");
    format!(
        "if command -v nc >/dev/null 2>&1; then nc -z -w {REMOTE_ENDPOINT_PREFLIGHT_TIMEOUT_SECS} {host} {port}; \
elif command -v python3 >/dev/null 2>&1; then python3 -c {python} {host} {port}; \
elif command -v bash >/dev/null 2>&1 && command -v timeout >/dev/null 2>&1; then timeout {REMOTE_ENDPOINT_PREFLIGHT_TIMEOUT_SECS} bash -c {bash} sh {host} {port}; \
else echo 'no TCP probe tool available on remote host (need nc, python3, or bash+timeout)' >&2; exit 127; fi"
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

/// Wrap a PowerShell script in the explicit
/// `powershell -NoProfile -NonInteractive -EncodedCommand` invocation used
/// for every Windows remote command, so nothing depends on the target sshd's
/// DefaultShell configuration.
///
/// The script is base64-encoded UTF-16LE, which survives every delivery layer
/// untouched: sshd feeds the command line to the target's default shell
/// (stock Win32-OpenSSH uses cmd.exe; some hosts set PowerShell or even
/// git-bash), and each of those re-parses quoting differently. The earlier
/// `-Command "…"` wrapper with `` `" `` escapes was verified to be mangled by
/// cmd.exe's argv parsing (inner quotes toggled instead of escaping), so the
/// payload is encoded instead of quoted — base64 contains no characters any
/// of these shells treat specially.
pub(crate) fn powershell_command(script: &str) -> String {
    let utf16_le: Vec<u8> = script
        .encode_utf16()
        .flat_map(|unit| unit.to_le_bytes())
        .collect();
    let encoded = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &utf16_le);
    format!("powershell -NoProfile -NonInteractive -EncodedCommand {encoded}")
}

/// Decode a command produced by [`powershell_command`] back into the
/// original script. Test-only helper so assertions can keep checking the
/// plain script content instead of the base64 payload.
#[cfg(test)]
pub(crate) fn decode_powershell_command(command: &str) -> String {
    let encoded = command
        .strip_prefix("powershell -NoProfile -NonInteractive -EncodedCommand ")
        .expect("command must use the EncodedCommand wrapper");
    let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, encoded.trim())
        .expect("EncodedCommand payload must be valid base64");
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
        .collect();
    String::from_utf16(&units).expect("EncodedCommand payload must be UTF-16LE")
}

/// Quote a value as a PowerShell single-quoted string literal.
pub(crate) fn ps_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn windows_release_zip_url(version: &str) -> String {
    format!(
        "{WAITAGENT_WINDOWS_RELEASE_BASE_URL}/v{version}/waitagent-{version}-x86_64-windows.zip"
    )
}

/// PowerShell guard emitting a clear error when curl.exe or tar.exe is
/// missing on the Windows target (both ship with Windows 10+).
fn missing_curl_or_tar_guard() -> String {
    "if (-not (Get-Command curl.exe -ErrorAction SilentlyContinue)) { Write-Error 'curl.exe is required to install WaitAgent on Windows but was not found on PATH'; exit 127 }; \
if (-not (Get-Command tar.exe -ErrorAction SilentlyContinue)) { Write-Error 'tar.exe is required to install WaitAgent on Windows but was not found on PATH'; exit 127 }; "
        .to_string()
}

/// Inject proxy settings into a PowerShell script as `$env:` assignments so
/// curl.exe picks them up (`docs/windows-ssh-target-design.md` §2).
fn push_proxy_assignments(script: &mut String, all_proxy: Option<&str>, https_proxy: Option<&str>) {
    if let Some(proxy) = non_empty(all_proxy) {
        script.push_str(&format!("$env:ALL_PROXY = {}; ", ps_single_quote(proxy)));
    }
    if let Some(proxy) = non_empty(https_proxy) {
        script.push_str(&format!("$env:HTTPS_PROXY = {}; ", ps_single_quote(proxy)));
    }
}

/// Windows installer body (task T3): download the release zip with curl.exe
/// and unpack it into `%LOCALAPPDATA%\Programs\waitagent\` with tar.exe
/// (bsdtar, which auto-detects zip archives), then stamp `version.txt`.
fn windows_install_body(
    all_proxy: Option<&str>,
    https_proxy: Option<&str>,
    version: &str,
) -> String {
    let url = windows_release_zip_url(version);
    let mut body = missing_curl_or_tar_guard();
    body.push_str("$dir = Split-Path $exe; New-Item -ItemType Directory -Force $dir | Out-Null; ");
    body.push_str("$tmp = Join-Path $env:TEMP ('waitagent-install-' + [guid]::NewGuid().ToString('N')); New-Item -ItemType Directory -Force $tmp | Out-Null; ");
    body.push_str("$zip = Join-Path $tmp 'waitagent.zip'; ");
    push_proxy_assignments(&mut body, all_proxy, https_proxy);
    body.push_str(&format!(
        "curl.exe -fsSL --retry 3 --connect-timeout 5 --max-time 120 -o $zip {}; ",
        ps_single_quote(&url)
    ));
    body.push_str(
        "if ($LASTEXITCODE -ne 0) { Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $tmp; exit 1 }; ",
    );
    body.push_str("tar.exe -xf $zip -C $dir; ");
    body.push_str(
        "if ($LASTEXITCODE -ne 0) { Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $tmp; exit 1 }; ",
    );
    body.push_str(&format!(
        "Set-Content -Path (Join-Path $dir 'version.txt') -Value {} -NoNewline; ",
        ps_single_quote(version)
    ));
    body.push_str("Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $tmp");
    body
}

/// Windows install command: skip the download when the installed binary
/// already reports the expected version, otherwise run the zip installer.
pub fn windows_install_or_update_command(
    all_proxy: Option<&str>,
    https_proxy: Option<&str>,
) -> String {
    let version = env!("CARGO_PKG_VERSION");
    let install = windows_install_body(all_proxy, https_proxy, version);
    let script = format!(
        "$exe = {WINDOWS_WAITAGENT_EXE_PS}; \
$current = $false; \
if (Test-Path $exe) {{ $out = & $exe --version 2>$null; if ($LASTEXITCODE -eq 0 -and \"$out\" -like {}) {{ $current = $true }} }}; \
if (-not $current) {{ {install} }}",
        ps_single_quote(&format!("*{version}*")),
    );
    powershell_command(&script)
}

fn windows_install_operator_public_key_command(public_key: &str, fingerprint: &str) -> String {
    let file_name = format!("{fingerprint}.pub");
    let script = format!(
        "$dir = \"{WINDOWS_AUTHORIZED_OPERATORS_PS}\"; \
New-Item -ItemType Directory -Force $dir | Out-Null; \
[System.IO.File]::WriteAllText((Join-Path $dir {}), {} + \"`n\"); \
if (Test-Path (Join-Path $dir {})) {{ exit 0 }} else {{ exit 1 }}",
        ps_single_quote(&file_name),
        ps_single_quote(public_key),
        ps_single_quote(&file_name),
    );
    powershell_command(&script)
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
        /// First matching substring wins and is consumed; canned stdout for
        /// commands that are not the credentials generator (e.g. the
        /// upload path's `uname` detection exec).
        stdout_by_substring: Rc<RefCell<Vec<(String, String)>>>,
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
            // Windows commands carry a base64-encoded payload; decode before
            // matching substrings so fixtures key off the plain script.
            let plain_command =
                if command.starts_with("powershell -NoProfile -NonInteractive -EncodedCommand ") {
                    decode_powershell_command(command)
                } else {
                    command.to_string()
                };
            let stdout = if plain_command.contains("__generate-node-credentials") {
                self.credentials_stdout
                    .borrow_mut()
                    .take()
                    .unwrap_or_default()
                    .into_bytes()
            } else {
                let mut canned = self.stdout_by_substring.borrow_mut();
                let index = canned
                    .iter()
                    .position(|(needle, _)| plain_command.contains(needle));
                index
                    .map(|index| canned.remove(index).1.into_bytes())
                    .unwrap_or_default()
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
            ssh_port: None,
            last_remote_port: None,
            last_endpoint: None,
            last_connected_at: None,
            use_install_proxy: true,
            tls_pin_sha256: None,
            ..RemoteHostProfile::default()
        };

        let plan = RemoteHostBootstrapPlan::from_profile(
            &profile,
            7476,
            "10.1.26.84:7474",
            "10.1.29.130#7476",
            RemoteShellKind::Posix,
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
    fn remote_host_bootstrapper_runs_powershell_flow_for_windows_shell() {
        use crate::infra::operator_auth::{MemoryOperatorKeyStore, OperatorKeyStore};
        let ssh_id = RemoteHostSecretId::new("waitagent.remote-host.win.ssh-password").unwrap();
        let sudo_id = RemoteHostSecretId::new("waitagent.remote-host.win.sudo-password").unwrap();
        let store = MemoryRemoteHostSecretStore::default();
        store
            .put_secret(&ssh_id, RemoteHostSecretValue::new("ssh-secret"))
            .unwrap();
        store
            .put_secret(&sudo_id, RemoteHostSecretValue::new("sudo-secret"))
            .unwrap();
        let profile = RemoteHostProfile {
            name: "windows-box".to_string(),
            host: "192.168.1.6".to_string(),
            ssh_user: "jj".to_string(),
            auth: RemoteHostAuthProfile::Password {
                password_secret_id: Some(ssh_id),
            },
            sudo_password_secret_id: Some(sudo_id),
            preferred_remote_port: RemotePortPreference::Auto,
            ssh_port: None,
            last_remote_port: None,
            last_endpoint: None,
            last_connected_at: None,
            use_install_proxy: true,
            tls_pin_sha256: None,
            remote_shell: Some(RemoteShellKind::Windows),
            ..RemoteHostProfile::default()
        };
        let plan = RemoteHostBootstrapPlan::from_profile(
            &profile,
            7476,
            "10.1.26.84:7474",
            "192.168.1.6#7476",
            RemoteShellKind::Windows,
        );
        let operator_key = MemoryOperatorKeyStore::generate().unwrap();
        let mut plan = plan;
        plan.operator_public_key = Some(operator_key.public_key_openssh().unwrap());
        let calls = Rc::new(RefCell::new(Vec::new()));
        let bootstrapper = SshRemoteHostBootstrapper::with_executor(
            store,
            RecordingSshExecutor {
                calls: calls.clone(),
                statuses: Rc::new(RefCell::new(vec![0, 1, 0, 0, 0, 1, 0])),
                credentials_stdout: Rc::new(RefCell::new(Some(
                    "WAITAGENT_CREDENTIALSdeadbeef:7476\n".to_string(),
                ))),
                stdout_by_substring: Rc::new(RefCell::new(Vec::new())),
            },
        );

        let result = bootstrapper.ensure_waitagent_and_start(&plan).unwrap();

        assert_eq!(result.tls_pin_sha256, "deadbeef");
        assert_eq!(result.remote_port, 7476);
        let calls = calls.borrow();
        assert_eq!(calls.len(), 7);
        for (target, command, stdin) in calls.iter() {
            assert_eq!(target.host, "192.168.1.6");
            assert_eq!(target.user, "jj");
            assert!(
                command.starts_with("powershell -NoProfile -NonInteractive -EncodedCommand "),
                "every windows remote command must be an explicit powershell exec: {command}"
            );
            assert_eq!(*stdin, None, "windows flow must never use sudo stdin");
        }
        let scripts: Vec<String> = calls
            .iter()
            .map(|(_, command, _)| decode_powershell_command(command))
            .collect();
        assert!(scripts[0].contains("New-Item"));
        assert!(scripts[0].contains("USERPROFILE"));
        assert!(scripts[1].contains("Test-Path"));
        assert!(scripts[1].contains("--version"));
        assert!(scripts[2].contains("curl.exe"));
        assert!(scripts[2].contains("tar.exe"));
        assert!(scripts[2].contains("LOCALAPPDATA"));
        assert!(scripts[2].contains("version.txt"));
        assert!(scripts[3].contains("__generate-node-credentials"));
        assert!(scripts[4].contains("authorized_operators"));
        assert!(scripts[5].contains("Get-CimInstance"));
        assert!(scripts[5].contains("--port 7476"));
        assert!(scripts[6].contains("__ratatui-node-server"));
        assert!(scripts[6].contains("--node-id '192.168.1.6#7476'"));
        assert!(!scripts[6].contains("nohup"));
        assert!(!scripts.iter().any(|script| script.contains("sudo")));
    }

    #[test]
    fn remote_host_bootstrapper_windows_inbound_mode_preflights_endpoint() {
        let ssh_id = RemoteHostSecretId::new("waitagent.remote-host.win.ssh-password").unwrap();
        let store = MemoryRemoteHostSecretStore::default();
        store
            .put_secret(&ssh_id, RemoteHostSecretValue::new("ssh-secret"))
            .unwrap();
        let profile = RemoteHostProfile {
            name: "windows-box".to_string(),
            host: "192.168.1.6".to_string(),
            ssh_user: "jj".to_string(),
            auth: RemoteHostAuthProfile::Password {
                password_secret_id: Some(ssh_id),
            },
            sudo_password_secret_id: None,
            preferred_remote_port: RemotePortPreference::Auto,
            ssh_port: None,
            last_remote_port: None,
            last_endpoint: None,
            last_connected_at: None,
            use_install_proxy: true,
            tls_pin_sha256: None,
            remote_shell: Some(RemoteShellKind::Windows),
            ..RemoteHostProfile::default()
        };
        let plan = RemoteHostBootstrapPlan::from_profile(
            &profile,
            7476,
            "192.168.31.178:7474",
            "192.168.1.6#7476",
            RemoteShellKind::Windows,
        );
        let mut plan = plan;
        // Force inbound mode so the local-endpoint preflight is exercised.
        plan.start_plan = RemoteWaitAgentStartPlan::new_with_mode(
            plan.start_plan.remote_port,
            plan.start_plan.local_connect_endpoint.clone(),
            plan.start_plan.authority_id.clone(),
            false,
            RemoteShellKind::Windows,
        );
        let calls = Rc::new(RefCell::new(Vec::new()));
        let bootstrapper = SshRemoteHostBootstrapper::with_executor(
            store,
            RecordingSshExecutor {
                calls: calls.clone(),
                statuses: Rc::new(RefCell::new(vec![1])),
                credentials_stdout: Rc::new(RefCell::new(None)),
                stdout_by_substring: Rc::new(RefCell::new(Vec::new())),
            },
        );

        let error = bootstrapper.ensure_waitagent_and_start(&plan).unwrap_err();

        assert!(error
            .to_string()
            .contains("remote host cannot reach local WaitAgent endpoint"));
        assert!(error.to_string().contains("--public <host:port>"));
        let calls = calls.borrow();
        assert_eq!(calls.len(), 1);
        let script = decode_powershell_command(&calls[0].1);
        assert!(script.contains("TcpClient"));
        assert!(script.contains("192.168.31.178"));
    }

    #[test]
    fn windows_plan_generates_powershell_install_start_and_credentials_commands() {
        let profile = RemoteHostProfile {
            name: "windows-box".to_string(),
            host: "192.168.1.6".to_string(),
            ssh_user: "jj".to_string(),
            auth: RemoteHostAuthProfile::Password {
                password_secret_id: None,
            },
            sudo_password_secret_id: None,
            preferred_remote_port: RemotePortPreference::Auto,
            ssh_port: None,
            last_remote_port: None,
            last_endpoint: None,
            last_connected_at: None,
            use_install_proxy: true,
            tls_pin_sha256: None,
            remote_shell: Some(RemoteShellKind::Windows),
            ..RemoteHostProfile::default()
        };

        let plan = RemoteHostBootstrapPlan::from_profile(
            &profile,
            7476,
            "10.1.26.84:7474",
            "192.168.1.6#7476",
            RemoteShellKind::Windows,
        );

        let version = env!("CARGO_PKG_VERSION");
        let install = decode_powershell_command(&plan.install_or_update_command);
        assert!(install.contains(&format!("waitagent-{version}-x86_64-windows.zip")));
        assert!(install.contains("curl.exe"));
        assert!(install.contains("tar.exe"));
        assert!(install.contains("LOCALAPPDATA"));
        assert!(plan.start_plan.endpoint_preflight_command.is_empty());
        let start = decode_powershell_command(&plan.start_plan.command);
        assert!(!start.contains("--connect"));
        assert!(start.contains("--node-id '192.168.1.6#7476'"));
        assert!(start.contains("__ratatui-node-server"));
        assert!(!start.contains("nohup"));
        let credentials = decode_powershell_command(&plan.start_plan.credentials_command);
        assert!(credentials.contains("__generate-node-credentials"));
        assert!(credentials.contains("node.key"));
        assert!(plan.start_plan.outbound_dial);
    }

    #[test]
    fn windows_install_command_injects_proxy_environment() {
        let command = windows_install_or_update_command(
            Some("socks5://127.0.0.1:7897"),
            Some("http://127.0.0.1:7897"),
        );

        let script = decode_powershell_command(&command);
        assert!(script.contains("$env:ALL_PROXY = 'socks5://127.0.0.1:7897'"));
        assert!(script.contains("$env:HTTPS_PROXY = 'http://127.0.0.1:7897'"));
        assert!(script.contains("curl.exe -fsSL"));
    }

    #[test]
    fn windows_install_reachability_preflight_uses_curl_and_nul_sink() {
        let command =
            windows_install_reachability_preflight_command(Some("socks5://127.0.0.1:7897"), None);

        let script = decode_powershell_command(&command);
        assert!(script.contains("curl.exe -fsSIL"));
        assert!(!script.contains("curl.exe -fsSL "));
        assert!(script.contains("-o NUL"));
        assert!(script.contains("$env:ALL_PROXY = 'socks5://127.0.0.1:7897'"));
        assert!(!script.contains("$env:HTTPS_PROXY"));
        assert!(script.contains("exit $LASTEXITCODE"));
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
            ssh_port: None,
            last_remote_port: None,
            last_endpoint: None,
            last_connected_at: None,
            use_install_proxy: true,
            tls_pin_sha256: None,
            ..RemoteHostProfile::default()
        };

        let plan = RemoteHostBootstrapPlan::from_profile(
            &profile,
            7476,
            "10.1.26.84:7474",
            "10.1.29.130#7476",
            RemoteShellKind::Posix,
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
            ssh_port: None,
            last_remote_port: None,
            last_endpoint: None,
            last_connected_at: None,
            use_install_proxy: true,
            tls_pin_sha256: None,
            ..RemoteHostProfile::default()
        };
        let plan = RemoteHostBootstrapPlan::from_profile(
            &profile,
            7476,
            "10.1.26.84:7474",
            "10.1.29.130#7476",
            RemoteShellKind::Posix,
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
                stdout_by_substring: Rc::new(RefCell::new(Vec::new())),
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
            ssh_port: None,
            last_remote_port: None,
            last_endpoint: None,
            last_connected_at: None,
            use_install_proxy: true,
            tls_pin_sha256: None,
            ..RemoteHostProfile::default()
        };
        let mut plan = RemoteHostBootstrapPlan::from_profile(
            &profile,
            7476,
            "10.1.26.84:7474",
            "10.1.29.130#7476",
            RemoteShellKind::Posix,
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
                stdout_by_substring: Rc::new(RefCell::new(Vec::new())),
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
            ssh_port: None,
            last_remote_port: None,
            last_endpoint: None,
            last_connected_at: None,
            use_install_proxy: true,
            tls_pin_sha256: None,
            ..RemoteHostProfile::default()
        };
        let mut plan = RemoteHostBootstrapPlan::from_profile(
            &profile,
            7476,
            "192.168.31.178:7474",
            "10.1.29.130#7476",
            RemoteShellKind::Posix,
        );
        // Force inbound mode so the local-endpoint preflight is exercised.
        plan.start_plan = RemoteWaitAgentStartPlan::new_with_mode(
            plan.start_plan.remote_port,
            plan.start_plan.local_connect_endpoint.clone(),
            plan.start_plan.authority_id.clone(),
            false,
            RemoteShellKind::Posix,
        );
        let calls = Rc::new(RefCell::new(Vec::new()));
        let bootstrapper = SshRemoteHostBootstrapper::with_executor(
            store,
            RecordingSshExecutor {
                calls: calls.clone(),
                statuses: Rc::new(RefCell::new(vec![1])),
                credentials_stdout: Rc::new(RefCell::new(None)),
                stdout_by_substring: Rc::new(RefCell::new(Vec::new())),
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
        let command = endpoint_preflight_command("127.0.0.1", RemoteShellKind::Posix);

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
            ssh_port: None,
            last_remote_port: None,
            last_endpoint: None,
            last_connected_at: None,
            use_install_proxy: true,
            tls_pin_sha256: None,
            ..RemoteHostProfile::default()
        };
        let plan = RemoteHostBootstrapPlan::from_profile(
            &profile,
            7476,
            "10.1.26.84:7474",
            "10.1.29.130#7476",
            RemoteShellKind::Posix,
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
                stdout_by_substring: Rc::new(RefCell::new(Vec::new())),
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
    fn remote_host_bootstrapper_installs_operator_key_without_sudo() {
        use crate::infra::operator_auth::{MemoryOperatorKeyStore, OperatorKeyStore};
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
            ssh_port: None,
            last_remote_port: None,
            last_endpoint: None,
            last_connected_at: None,
            use_install_proxy: true,
            tls_pin_sha256: None,
            ..RemoteHostProfile::default()
        };
        let plan = RemoteHostBootstrapPlan::from_profile(
            &profile,
            7476,
            "10.1.26.84:7474",
            "10.1.29.130#7476",
            RemoteShellKind::Posix,
        );
        let operator_key = MemoryOperatorKeyStore::generate().unwrap();
        let mut plan = plan;
        plan.operator_public_key = Some(operator_key.public_key_openssh().unwrap());
        let calls = Rc::new(RefCell::new(Vec::new()));
        let bootstrapper = SshRemoteHostBootstrapper::with_executor(
            store,
            RecordingSshExecutor {
                calls: calls.clone(),
                statuses: Rc::new(RefCell::new(vec![0, 1, 0, 0, 0, 1, 0])),
                credentials_stdout: Rc::new(RefCell::new(Some(
                    "WAITAGENT_CREDENTIALSdeadbeef:7476\n".to_string(),
                ))),
                stdout_by_substring: Rc::new(RefCell::new(Vec::new())),
            },
        );

        bootstrapper.ensure_waitagent_and_start(&plan).unwrap();

        let calls = calls.borrow();
        assert_eq!(calls.len(), 7);
        let install = calls
            .iter()
            .find(|(_, command, _)| command.contains("authorized_operators"))
            .expect("operator key install command must run");
        assert!(
            !install.1.contains("sudo"),
            "operator key install must stay in the target user's $HOME, not sudo: {}",
            install.1
        );
        assert!(
            install.2.is_none(),
            "operator key install must not consume the sudo password stdin"
        );
        let update = calls
            .iter()
            .find(|(_, command, _)| command.contains("sudo -S"))
            .expect("system install command must run with sudo");
        assert!(
            update.2.is_some(),
            "sudo install must consume the sudo password stdin"
        );
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
            ssh_port: None,
            last_remote_port: None,
            last_endpoint: None,
            last_connected_at: None,
            use_install_proxy: true,
            tls_pin_sha256: None,
            ..RemoteHostProfile::default()
        };
        let plan = RemoteHostBootstrapPlan::from_profile(
            &profile,
            7476,
            "10.1.26.84:7474",
            "10.1.29.130#7476",
            RemoteShellKind::Posix,
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
                stdout_by_substring: Rc::new(RefCell::new(Vec::new())),
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
            ssh_port: None,
            last_remote_port: None,
            last_endpoint: None,
            last_connected_at: None,
            use_install_proxy: true,
            tls_pin_sha256: None,
            ..RemoteHostProfile::default()
        };

        let plan = RemoteHostBootstrapPlan::from_profile(
            &profile,
            7476,
            "10.1.26.84:7474",
            "10.1.29.130#7476",
            RemoteShellKind::Posix,
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

    fn upload_plan() -> RemoteHostBootstrapPlan {
        let profile = RemoteHostProfile {
            name: "130".to_string(),
            host: "10.1.29.130".to_string(),
            ssh_user: "kk".to_string(),
            auth: RemoteHostAuthProfile::Key {
                key_path: std::path::PathBuf::from("/home/kk/.ssh/id_ed25519"),
            },
            sudo_password_secret_id: None,
            preferred_remote_port: RemotePortPreference::Auto,
            ssh_port: None,
            last_remote_port: None,
            last_endpoint: None,
            last_connected_at: None,
            use_install_proxy: true,
            tls_pin_sha256: None,
            install_source: InstallSource::LocalUpload,
            ..RemoteHostProfile::default()
        };
        let mut plan = RemoteHostBootstrapPlan::from_profile(
            &profile,
            7476,
            "10.1.26.84:7474",
            "10.1.29.130#7476",
            RemoteShellKind::Posix,
        );
        plan.install_source = InstallSource::LocalUpload;
        plan
    }

    /// Builds a valid release-layout artifact (tar.gz with a top-level
    /// `waitagent` entry) and primes the cache directory the way a real
    /// first download would, including the sha256 sidecar.
    fn prime_artifact_cache() -> LocalArtifactCache {
        let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        let mut builder = tar::Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        let body = b"#!/bin/sh\necho waitagent 0.1.90\n";
        header.set_size(body.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder
            .append_data(&mut header, "waitagent", &body[..])
            .expect("append waitagent entry");
        let encoder = builder.into_inner().expect("finish tar archive");
        let artifact = encoder.finish().expect("finish gzip stream");
        let dir = std::path::PathBuf::from(format!(
            "{}/waitagent-upload-cache-{}-{}",
            std::env::temp_dir().display(),
            std::process::id(),
            std::thread::current()
                .name()
                .unwrap_or("test")
                .replace(':', "_")
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let file_name = ArtifactTarget::LinuxX86_64.file_name(env!("CARGO_PKG_VERSION"));
        std::fs::write(dir.join(&file_name), &artifact).unwrap();
        let digest = {
            use sha2::Digest;
            sha2::Sha256::digest(&artifact)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        };
        std::fs::write(
            dir.join(format!("{file_name}.sha256")),
            format!("{digest}  {file_name}\n"),
        )
        .unwrap();
        LocalArtifactCache::new(dir)
    }

    #[test]
    fn parse_upload_detection_maps_uname_and_uid() {
        let detection = parse_upload_detection(b"Linux\nx86_64\n0\n").unwrap();
        assert_eq!(detection.target, ArtifactTarget::LinuxX86_64);
        assert_eq!(detection.uid, 0);

        let detection = parse_upload_detection(b"Darwin\narm64\n501\n").unwrap();
        assert_eq!(detection.target, ArtifactTarget::MacosAArch64);
        assert_eq!(detection.uid, 501);
    }

    #[test]
    fn parse_upload_detection_rejects_garbage_and_unsupported_targets() {
        let error = parse_upload_detection(b"Linux\nx86_64\nnot-a-uid\n").unwrap_err();
        assert!(error.to_string().contains("unparsable uid"));

        let error = parse_upload_detection(b"Linux\nriscv64\n0\n").unwrap_err();
        assert!(
            error.to_string().contains("unsupported remote target"),
            "the error names the unsupported pair: {error}"
        );

        let error = parse_upload_detection(b"Linux\n").unwrap_err();
        assert!(error.to_string().contains("unparsable uid"));
    }

    #[test]
    fn upload_chunks_split_and_append_via_stdin() {
        let data = vec![7_u8; UPLOAD_CHUNK_BYTES * 2 + 1];
        let chunks = upload_chunks(7476, &data);
        assert_eq!(chunks.len(), 3);
        let b64_path = shell_single_quote(&upload_b64_path(7476));
        assert_eq!(chunks[0].0, format!("cat > {b64_path}"));
        assert_eq!(chunks[1].0, format!("cat >> {b64_path}"));
        assert_eq!(chunks[2].0, format!("cat >> {b64_path}"));
        // The command line stays tiny; the payload rides the exec's stdin —
        // sshd rejects command lines close to one chunk's base64 size.
        assert!(chunks[0].0.len() < 64);
        // Every stdin payload decodes back to its source chunk.
        let engine = base64::engine::general_purpose::STANDARD;
        let mut decoded = Vec::new();
        for (_, payload) in &chunks {
            decoded.extend_from_slice(
                &base64::Engine::decode(&engine, payload).expect("valid base64 payload"),
            );
        }
        assert_eq!(decoded, data);
    }

    #[test]
    fn upload_install_command_keeps_install_sh_steps() {
        let plan = upload_plan();
        let command = upload_install_command(
            &plan,
            ArtifactTarget::LinuxX86_64,
            "/usr/local/bin",
            "/usr/local/bin/waitagent",
        );
        assert!(command.contains("base64 -d /tmp/.waitagent-upload-7476.b64"));
        assert!(command.contains("tar xzf /tmp/.waitagent-upload-7476.tar.gz"));
        assert!(command.contains("test -f \"$tmpd/waitagent\""));
        assert!(command.contains("mkdir -p \"/usr/local/bin\""));
        assert!(command.contains("chmod 755"));
        assert!(
            command.contains("mv -f \"/usr/local/bin/.waitagent.tmp\" '/usr/local/bin/waitagent'")
        );
        assert!(command.contains("setcap cap_net_admin+ep"));
        assert!(command.contains("rm -rf \"$tmpd\""));
        assert!(command.contains("'/usr/local/bin/waitagent' --version 2>/dev/null | grep -q"));
        assert!(command.contains(env!("CARGO_PKG_VERSION")));

        let macos = upload_install_command(
            &plan,
            ArtifactTarget::MacosAArch64,
            "/usr/local/bin",
            "/usr/local/bin/waitagent",
        );
        assert!(
            !macos.contains("setcap"),
            "macOS assets skip the Linux capability step"
        );
    }

    #[test]
    fn upload_commands_quote_home_fallback_for_remote_expansion() {
        let plan = upload_plan();
        let credentials = upload_credentials_command(&plan, "$HOME/.local/bin/waitagent");
        assert!(
            credentials.starts_with("\"$HOME/.local/bin/waitagent\" --port"),
            "$HOME paths must stay double-quoted for remote expansion: {credentials}"
        );
        let start = upload_start_command(&plan, "$HOME/.local/bin/waitagent");
        assert!(start.contains("nohup \"$HOME/.local/bin/waitagent\" --port 7476"));
        assert!(start.contains(">/tmp/waitagent-7476.log"));
        assert!(start.contains("__ratatui-node-server"));
        let system = upload_install_command(
            &plan,
            ArtifactTarget::LinuxX86_64,
            "$HOME/.local/bin",
            "$HOME/.local/bin/waitagent",
        );
        assert!(system.contains("mkdir -p \"$HOME/.local/bin\""));
        assert!(system
            .contains("mv -f \"$HOME/.local/bin/.waitagent.tmp\" \"$HOME/.local/bin/waitagent\""));
    }

    #[test]
    fn upload_start_command_supports_inbound_connect_mode() {
        let mut plan = upload_plan();
        plan.start_plan = RemoteWaitAgentStartPlan::new_with_mode(
            plan.start_plan.remote_port,
            plan.start_plan.local_connect_endpoint.clone(),
            plan.start_plan.authority_id.clone(),
            false,
            RemoteShellKind::Posix,
        );
        let start = upload_start_command(&plan, "/usr/local/bin/waitagent");
        assert!(start.contains("--connect '10.1.26.84:7474'"));
        assert!(start.contains("--node-id '10.1.29.130#7476'"));
    }

    #[test]
    fn upload_flow_installs_from_primed_cache() {
        let cache = prime_artifact_cache();
        let calls = Rc::new(RefCell::new(Vec::new()));
        // Statuses pop from the end: mkdir, uname, version check (not
        // current), one chunk, install, credentials, daemon check (not
        // running), start.
        let bootstrapper = SshRemoteHostBootstrapper::with_executor(
            MemoryRemoteHostSecretStore::default(),
            RecordingSshExecutor {
                calls: calls.clone(),
                // Statuses pop from the end: start, daemon check (not
                // running), credentials, install, one chunk, version check
                // (not current), uname, mkdir.
                statuses: Rc::new(RefCell::new(vec![0, 1, 0, 0, 0, 1, 0, 0])),
                credentials_stdout: Rc::new(RefCell::new(Some(
                    "WAITAGENT_CREDENTIALSdeadbeef:7476\n".to_string(),
                ))),
                stdout_by_substring: Rc::new(RefCell::new(vec![(
                    "uname -s".to_string(),
                    "Linux\nx86_64\n0\n".to_string(),
                )])),
            },
        )
        .with_artifact_cache(cache);

        let result = bootstrapper
            .ensure_waitagent_and_start(&upload_plan())
            .unwrap();

        assert_eq!(result.tls_pin_sha256, "deadbeef");
        assert_eq!(result.remote_port, 7476);
        let calls = calls.borrow();
        assert_eq!(
            calls.len(),
            8,
            "mkdir, uname, version, chunk, install, credentials, daemon, start: {calls:?}"
        );
        assert!(calls[0].1.contains("mkdir -p"));
        assert_eq!(calls[1].1, "uname -s && uname -m && id -u");
        assert_eq!(calls[1].2, None);
        assert!(
            calls[2].1.contains("'/usr/local/bin/waitagent' --version"),
            "version gate checks the explicit upload install path: {}",
            calls[2].1
        );
        assert!(
            calls[3].1.starts_with("cat > ") && calls[3].1.contains("waitagent-upload-7476.b64"),
            "first chunk truncates the upload temp file: {}",
            calls[3].1
        );
        let chunk_payload = calls[3]
            .2
            .as_deref()
            .expect("chunk payload travels on stdin");
        base64::Engine::decode(&base64::engine::general_purpose::STANDARD, chunk_payload)
            .expect("valid base64 chunk");
        assert!(calls[4].1.contains("base64 -d"));
        assert!(calls[4].1.contains("setcap cap_net_admin+ep"));
        assert!(calls[5].1.contains("__generate-node-credentials"));
        assert!(
            calls[5].1.starts_with("'/usr/local/bin/waitagent' --port"),
            "credentials command uses the explicit install path: {}",
            calls[5].1
        );
        assert!(calls[6].1.contains("ps -eo args="));
        assert!(
            calls[7]
                .1
                .contains("nohup '/usr/local/bin/waitagent' --port 7476"),
            "start command uses the explicit install path: {}",
            calls[7].1
        );
        assert!(
            !calls.iter().any(|(_, command, _)| command.contains("sudo")),
            "root upload installs never invoke sudo"
        );
    }

    #[test]
    fn upload_flow_skips_install_when_remote_is_current() {
        let cache = prime_artifact_cache();
        let calls = Rc::new(RefCell::new(Vec::new()));
        let bootstrapper = SshRemoteHostBootstrapper::with_executor(
            MemoryRemoteHostSecretStore::default(),
            RecordingSshExecutor {
                calls: calls.clone(),
                statuses: Rc::new(RefCell::new(vec![0, 0, 0, 0, 0])),
                credentials_stdout: Rc::new(RefCell::new(Some(
                    "WAITAGENT_CREDENTIALSdeadbeef:7476\n".to_string(),
                ))),
                stdout_by_substring: Rc::new(RefCell::new(vec![(
                    "uname -s".to_string(),
                    "Linux\nx86_64\n0\n".to_string(),
                )])),
            },
        )
        .with_artifact_cache(cache);

        bootstrapper
            .ensure_waitagent_and_start(&upload_plan())
            .unwrap();

        let calls = calls.borrow();
        assert_eq!(
            calls.len(),
            5,
            "mkdir, uname, version, credentials, daemon: {calls:?}"
        );
        assert!(
            !calls.iter().any(|(_, command, _)| {
                command.contains("base64 -d")
                    || command.starts_with("cat > ")
                    || command.starts_with("cat >> ")
            }),
            "a current remote must not upload anything"
        );
    }

    #[test]
    fn upload_flow_rejects_windows_without_any_exec() {
        let mut plan = upload_plan();
        plan.remote_shell = RemoteShellKind::Windows;
        let calls = Rc::new(RefCell::new(Vec::new()));
        let bootstrapper = SshRemoteHostBootstrapper::with_executor(
            MemoryRemoteHostSecretStore::default(),
            RecordingSshExecutor {
                calls: calls.clone(),
                statuses: Rc::new(RefCell::new(Vec::new())),
                credentials_stdout: Rc::new(RefCell::new(None)),
                stdout_by_substring: Rc::new(RefCell::new(Vec::new())),
            },
        );

        let error = bootstrapper.ensure_waitagent_and_start(&plan).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("not supported for Windows remote hosts"),
            "typed, actionable error: {error}"
        );
        assert!(
            error
                .to_string()
                .contains("switch Install Source to Remote"),
            "the error tells the user how to fix it: {error}"
        );
        assert_eq!(calls.borrow().len(), 0, "no SSH exec may run");
    }
}
