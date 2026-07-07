use crate::domain::agent_detector::DetectorRegistry;
use crate::domain::session_catalog::{ManagedSessionAddress, ManagedSessionRecord};
use crate::domain::workspace::{
    WorkspaceInstanceConfig, WorkspaceInstanceId, WorkspaceSessionRole,
};
use crate::infra::tmux_error::{
    parse_tmux_identifier, tmux_socket_dir, validate_percent, TmuxCommandOutput, TmuxCommandRunner,
    TmuxError,
};
use crate::infra::tmux_glue::{
    TmuxGlueArtifacts, TmuxGlueBuildConfig, TmuxGlueBuildStatus, VendoredTmuxSource,
};
use crate::infra::tmux_types::{
    TmuxLayoutGateway, TmuxPaneId, TmuxProgram, TmuxSessionGateway, TmuxSessionName,
    TmuxSocketName, TmuxWorkspaceHandle,
};
use crate::runtime::remote_authority_target_host_runtime::RemoteTargetTerminalFlags;
use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
#[cfg(not(test))]
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

mod control;
mod layout;
mod remote;
mod session_metadata;

const WAITAGENT_SOCKET_PREFIX: &str = "wa-";
const SYSTEM_TMUX_PROGRAM: &str = "tmux";
const WAITAGENT_WORKSPACE_DIR_ENV: &str = "WAITAGENT_WORKSPACE_DIR";
const WAITAGENT_WORKSPACE_KEY_ENV: &str = "WAITAGENT_WORKSPACE_KEY";
const WAITAGENT_SESSION_ROLE_ENV: &str = "WAITAGENT_SESSION_ROLE";
const WAITAGENT_TRANSPORT_ENV: &str = "WAITAGENT_SESSION_TRANSPORT";
const WAITAGENT_TRANSPORT_LOCAL_TMUX: &str = "local-tmux";
pub(crate) const WAITAGENT_PANE_PIPE_OWNER_OPTION: &str = "@waitagent_pane_pipe_owner";
pub(crate) const WAITAGENT_PANE_ROLE_OPTION: &str = "@waitagent_pane_role";
pub(crate) const WAITAGENT_PANE_ROLE_CONTENT: &str = "content";
pub(crate) const WAITAGENT_PANE_SESSION_INSTANCE_OPTION: &str = "@waitagent_session_instance_id";
pub(crate) const WAITAGENT_PANE_TARGET_SESSION_OPTION: &str = "@waitagent_target_session_name";
pub(crate) const WAITAGENT_PANE_TARGET_ID_OPTION: &str = "@waitagent_target_id";
pub(crate) const WAITAGENT_RUNTIME_COMMAND_OVERRIDE_OPTION: &str =
    "@waitagent_runtime_command_override";
pub(crate) const WAITAGENT_RUNTIME_RUNNING_OVERRIDE: &str = "__waitagent_running__";
pub(crate) const WAITAGENT_AGENT_SIGNAL_AGENT_OPTION: &str = "@waitagent_agent_signal_agent";
pub(crate) const WAITAGENT_AGENT_SIGNAL_PANE_OPTION: &str = "@waitagent_agent_signal_pane";
pub(crate) const WAITAGENT_AGENT_SIGNAL_STATE_OPTION: &str = "@waitagent_agent_signal_state";
pub(crate) const WAITAGENT_AGENT_SIGNAL_TOKEN_OPTION: &str = "@waitagent_agent_signal_token";
pub(crate) const WAITAGENT_AGENT_SIGNAL_UPDATED_AT_OPTION: &str =
    "@waitagent_agent_signal_updated_at";
pub(crate) const WAITAGENT_REMOTE_PUBLICATION_AUTHORITY_ID_ENV: &str =
    "WAITAGENT_REMOTE_PUBLICATION_AUTHORITY_ID";
pub(crate) const WAITAGENT_REMOTE_PUBLICATION_TRANSPORT_SESSION_ID_ENV: &str =
    "WAITAGENT_REMOTE_PUBLICATION_TRANSPORT_SESSION_ID";
pub(crate) const WAITAGENT_REMOTE_PUBLICATION_SELECTOR_ENV: &str =
    "WAITAGENT_REMOTE_PUBLICATION_SELECTOR";
pub(crate) const WAITAGENT_SIDEBAR_PANE_TITLE: &str = "waitagent-sidebar";
pub(crate) const WAITAGENT_FOOTER_PANE_TITLE: &str = "waitagent-footer";
const WAITAGENT_SIDEBAR_READY_CHANNEL_PREFIX: &str = "waitagent-sidebar-ready";
const WAITAGENT_FOOTER_READY_CHANNEL_PREFIX: &str = "waitagent-footer-ready";
const WAITAGENT_SIDEBAR_READY_OPTION: &str = "@waitagent_sidebar_ready_pane";
const WAITAGENT_FOOTER_READY_OPTION: &str = "@waitagent_footer_ready_pane";
const DEFAULT_HISTORY_LIMIT: &str = "100000";

thread_local! {
    static CHROME_REFRESH_SUBSCRIBERS: RefCell<HashMap<PathBuf, UnixStream>> =
        RefCell::new(HashMap::new());
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddedTmuxBackend {
    source: VendoredTmuxSource,
    artifacts: TmuxGlueArtifacts,
    build_status: TmuxGlueBuildStatus,
    build_config: TmuxGlueBuildConfig,
    registry: Arc<DetectorRegistry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaitagentSessionListEntry {
    pub socket_name: String,
    pub session_name: String,
    pub attached_clients: usize,
    pub window_count: usize,
    pub created_at_unix_secs: Option<u64>,
    pub session_role: Option<WorkspaceSessionRole>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WaitagentSocketCleanupReport {
    pub live: usize,
    pub removed: usize,
}

impl WaitagentSessionListEntry {
    pub fn display_session_id(&self) -> &str {
        self.session_name
            .strip_prefix("waitagent-")
            .unwrap_or(self.session_name.as_str())
    }

    pub fn role_tag(&self) -> &'static str {
        match self.session_role {
            Some(WorkspaceSessionRole::WorkspaceChrome) => " [main]",
            Some(WorkspaceSessionRole::TargetHost) => " [target]",
            _ => "",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct TmuxSessionMetadata {
    workspace_dir: Option<PathBuf>,
    workspace_key: Option<String>,
    session_role: Option<WorkspaceSessionRole>,
    remote_publication_authority_id: Option<String>,
    remote_publication_transport_session_id: Option<String>,
    remote_publication_selector: Option<String>,
}

impl EmbeddedTmuxBackend {
    pub fn new(
        source: VendoredTmuxSource,
        artifacts: TmuxGlueArtifacts,
        build_status: TmuxGlueBuildStatus,
        build_config: TmuxGlueBuildConfig,
        registry: DetectorRegistry,
    ) -> Self {
        Self {
            source,
            artifacts,
            build_status,
            build_config,
            registry: Arc::new(registry),
        }
    }

    #[allow(dead_code)]
    pub fn source(&self) -> &VendoredTmuxSource {
        &self.source
    }

    #[allow(dead_code)]
    pub fn build_config(&self) -> &TmuxGlueBuildConfig {
        &self.build_config
    }

    #[allow(dead_code)]
    pub fn artifacts(&self) -> &TmuxGlueArtifacts {
        &self.artifacts
    }

    #[allow(dead_code)]
    pub fn build_status(&self) -> &TmuxGlueBuildStatus {
        &self.build_status
    }

    pub fn from_build_env() -> Result<Self, TmuxError> {
        // 1. Try the embedded vendored tmux (extracted from the binary at runtime).
        //    This works regardless of install path or platform packaging.
        if let Ok(backend) = Self::embedded() {
            return Ok(backend);
        }
        // 2. Try vendored from build env (compile-time hardcoded paths).
        //    This only works on the exact machine where waitagent was built.
        match Self::vendored_from_build_env() {
            Ok(backend) => Ok(backend),
            Err(_) => Ok(Self::system_default()),
        }
    }

    fn embedded() -> Result<Self, TmuxError> {
        let tmux_binary_path = crate::infra::tmux_glue::extract_embedded_tmux()?;
        let source = VendoredTmuxSource::new(tmux_binary_path.clone());
        let artifacts = TmuxGlueArtifacts {
            source_path: tmux_binary_path.clone(),
            build_root: PathBuf::new(),
            tmux_binary_path,
            static_lib_path: PathBuf::new(),
            include_dir_path: PathBuf::new(),
            configure_stamp_path: PathBuf::new(),
            build_stamp_path: PathBuf::new(),
        };
        let build_config = TmuxGlueBuildConfig::from_artifacts(&artifacts);
        let backend = Self::new(
            source,
            artifacts,
            TmuxGlueBuildStatus::Executed,
            build_config,
            DetectorRegistry::default(),
        );
        backend.validate_runtime_artifacts()?;
        Ok(backend)
    }

    fn vendored_from_build_env() -> Result<Self, TmuxError> {
        let source = VendoredTmuxSource::discover_from_build_env()?;
        let artifacts = TmuxGlueArtifacts::from_build_env()?;
        let build_status = TmuxGlueBuildStatus::from_build_env()?;
        let build_config = TmuxGlueBuildConfig::from_artifacts(&artifacts);
        let backend = Self::new(
            source,
            artifacts,
            build_status,
            build_config,
            DetectorRegistry::default(),
        );
        backend.validate_runtime_artifacts()?;
        Ok(backend)
    }

    fn system_default() -> Self {
        let source = VendoredTmuxSource::system_default();
        let artifacts = TmuxGlueArtifacts::system_default();
        let build_config = TmuxGlueBuildConfig::from_artifacts(&artifacts);
        Self::new(
            source,
            artifacts,
            TmuxGlueBuildStatus::Executed,
            build_config,
            DetectorRegistry::default(),
        )
    }

    fn validate_runtime_artifacts(&self) -> Result<(), TmuxError> {
        if self.artifacts.tmux_binary_path == Path::new(SYSTEM_TMUX_PROGRAM) {
            return Ok(());
        }
        if self.build_status != TmuxGlueBuildStatus::Executed {
            return Err(TmuxError::new(format!(
                "vendored tmux build is not executable yet: build status is `{}`",
                self.build_status.as_str()
            )));
        }
        if !self.source.path().exists() {
            return Err(TmuxError::new(format!(
                "vendored tmux source is missing at {}",
                self.source.path().display()
            )));
        }
        if !self.artifacts.tmux_binary_path.exists() {
            return Err(TmuxError::new(format!(
                "vendored tmux binary is missing at {}",
                self.artifacts.tmux_binary_path.display()
            )));
        }
        Ok(())
    }

    fn command_runner(&self) -> TmuxCommandRunner {
        TmuxCommandRunner::new(self.artifacts.tmux_binary_path.clone())
    }

    fn workspace_handle_for_config(config: &WorkspaceInstanceConfig) -> TmuxWorkspaceHandle {
        TmuxWorkspaceHandle {
            workspace_id: WorkspaceInstanceId::new(config.workspace_key.clone()),
            socket_name: TmuxSocketName::new(config.socket_name.clone()),
            session_name: TmuxSessionName::new(config.session_name.clone()),
        }
    }

    fn run_workspace_command(
        &self,
        workspace: &TmuxWorkspaceHandle,
        args: &[String],
    ) -> Result<TmuxCommandOutput, TmuxError> {
        self.command_runner().run(&workspace.socket_name, args)
    }

    pub(crate) fn run_on_socket(
        &self,
        socket_name: &TmuxSocketName,
        args: &[String],
    ) -> Result<TmuxCommandOutput, TmuxError> {
        self.command_runner().run(socket_name, args)
    }

    pub(crate) fn run_socket_command(
        &self,
        socket_name: &TmuxSocketName,
        args: &[String],
    ) -> Result<(), TmuxError> {
        self.run_on_socket(socket_name, args).map(|_| ())
    }

    pub(crate) fn socket_is_live(&self, socket_name: &TmuxSocketName) -> bool {
        self.run_on_socket(socket_name, &["list-sessions".to_string()])
            .is_ok()
    }

    pub(crate) fn show_session_option(
        &self,
        workspace: &TmuxWorkspaceHandle,
        option_name: &str,
    ) -> Result<Option<String>, TmuxError> {
        let output = self.run_workspace_command(
            workspace,
            &[
                "show-options".to_string(),
                "-qv".to_string(),
                "-t".to_string(),
                exact_session_target(workspace.session_name.as_str()),
                option_name.to_string(),
            ],
        )?;
        let value = output.stdout.trim();
        if value.is_empty() {
            Ok(None)
        } else {
            Ok(Some(value.to_string()))
        }
    }

    pub(crate) fn clear_session_option(
        &self,
        workspace: &TmuxWorkspaceHandle,
        option_name: &str,
    ) -> Result<(), TmuxError> {
        self.run_workspace_command(
            workspace,
            &[
                "set-option".to_string(),
                "-qu".to_string(),
                "-t".to_string(),
                exact_session_target(workspace.session_name.as_str()),
                option_name.to_string(),
            ],
        )?;
        Ok(())
    }

    pub(crate) fn show_pane_option_on_socket(
        &self,
        socket_name: &TmuxSocketName,
        pane: &TmuxPaneId,
        option_name: &str,
    ) -> Result<Option<String>, TmuxError> {
        let output = self.run_on_socket(
            socket_name,
            &[
                "show-options".to_string(),
                "-pqv".to_string(),
                "-t".to_string(),
                pane.as_str().to_string(),
                option_name.to_string(),
            ],
        )?;
        let value = output.stdout.trim();
        if value.is_empty() {
            Ok(None)
        } else {
            Ok(Some(value.to_string()))
        }
    }

    pub(crate) fn set_pane_option_on_socket(
        &self,
        socket_name: &TmuxSocketName,
        pane: &TmuxPaneId,
        option_name: &str,
        value: &str,
    ) -> Result<(), TmuxError> {
        self.run_on_socket(
            socket_name,
            &[
                "set-option".to_string(),
                "-p".to_string(),
                "-t".to_string(),
                pane.as_str().to_string(),
                option_name.to_string(),
                value.to_string(),
            ],
        )?;
        Ok(())
    }

    pub(crate) fn unset_pane_option_on_socket(
        &self,
        socket_name: &TmuxSocketName,
        pane: &TmuxPaneId,
        option_name: &str,
    ) -> Result<(), TmuxError> {
        self.run_on_socket(
            socket_name,
            &[
                "set-option".to_string(),
                "-pu".to_string(),
                "-t".to_string(),
                pane.as_str().to_string(),
                option_name.to_string(),
            ],
        )?;
        Ok(())
    }

    pub(crate) fn set_global_option_on_socket(
        &self,
        socket_name: &TmuxSocketName,
        option_name: &str,
        value: &str,
    ) -> Result<(), TmuxError> {
        self.run_on_socket(
            socket_name,
            &[
                "set-option".to_string(),
                "-gq".to_string(),
                option_name.to_string(),
                value.to_string(),
            ],
        )?;
        Ok(())
    }

    pub(crate) fn show_global_option_on_socket(
        &self,
        socket_name: &TmuxSocketName,
        option_name: &str,
    ) -> Result<Option<String>, TmuxError> {
        let output = self.run_on_socket(
            socket_name,
            &[
                "show-options".to_string(),
                "-gqv".to_string(),
                option_name.to_string(),
            ],
        )?;
        let value = output.stdout.trim();
        if value.is_empty() {
            Ok(None)
        } else {
            Ok(Some(value.to_string()))
        }
    }

    /// Queries just the workspace directory for a session, without doing a full
    /// session listing. Used by the activation path to avoid redundant listings.
    pub(crate) fn session_workspace_dir(
        &self,
        socket_name: &TmuxSocketName,
        session_name: &str,
    ) -> Result<Option<PathBuf>, TmuxError> {
        let args = vec![
            "show-environment".to_string(),
            "-t".to_string(),
            exact_session_target(session_name),
        ];
        let output = self.run_on_socket(socket_name, &args)?;
        for line in output.stdout.lines() {
            if let Some((key, value)) = line.split_once('=') {
                if key == WAITAGENT_WORKSPACE_DIR_ENV {
                    return Ok(Some(PathBuf::from(value)));
                }
            }
        }
        Ok(None)
    }

    #[allow(dead_code)]
    pub(crate) fn show_session_local_option_names(
        &self,
        workspace: &TmuxWorkspaceHandle,
        option_name: &str,
    ) -> Result<Vec<String>, TmuxError> {
        let output = self.run_workspace_command(
            workspace,
            &[
                "show-options".to_string(),
                "-q".to_string(),
                "-t".to_string(),
                exact_session_target(workspace.session_name.as_str()),
                option_name.to_string(),
            ],
        )?;
        Ok(output
            .stdout
            .lines()
            .filter_map(|line| {
                line.split_once(' ')
                    .map(|(name, _)| name.trim().to_string())
            })
            .filter(|name| !name.is_empty())
            .collect())
    }

    #[allow(dead_code)]
    pub(crate) fn unset_session_option(
        &self,
        workspace: &TmuxWorkspaceHandle,
        option_name: &str,
    ) -> Result<(), TmuxError> {
        self.run_workspace_command(
            workspace,
            &[
                "set-option".to_string(),
                "-u".to_string(),
                "-t".to_string(),
                exact_session_target(workspace.session_name.as_str()),
                option_name.to_string(),
            ],
        )
        .map(|_| ())
    }

    fn session_exists(&self, workspace: &TmuxWorkspaceHandle) -> Result<bool, TmuxError> {
        let args = vec![
            "has-session".to_string(),
            "-t".to_string(),
            workspace.session_name.as_str().to_string(),
        ];
        match self.run_workspace_command(workspace, &args) {
            Ok(_) => Ok(true),
            Err(error) if error.is_command_failure() => Ok(false),
            Err(error) => Err(error),
        }
    }

    fn create_workspace_session(
        &self,
        config: &WorkspaceInstanceConfig,
        workspace: &TmuxWorkspaceHandle,
    ) -> Result<(), TmuxError> {
        let window_name = default_window_name();
        let mut args = vec![
            "set-option".to_string(),
            "-g".to_string(),
            "history-limit".to_string(),
            DEFAULT_HISTORY_LIMIT.to_string(),
            ";".to_string(),
            "set-option".to_string(),
            "-g".to_string(),
            "default-terminal".to_string(),
            "tmux-256color".to_string(),
            ";".to_string(),
            "set-option".to_string(),
            "-g".to_string(),
            "allow-passthrough".to_string(),
            "on".to_string(),
            ";".to_string(),
            "new-session".to_string(),
            "-d".to_string(),
            "-s".to_string(),
            workspace.session_name.as_str().to_string(),
            "-n".to_string(),
            window_name,
            "-c".to_string(),
            config.workspace_dir.display().to_string(),
        ];
        if let Some(cols) = config.initial_cols {
            args.push("-x".to_string());
            args.push(cols.to_string());
        }
        if let Some(rows) = config.initial_rows {
            args.push("-y".to_string());
            args.push(rows.to_string());
        }
        args.extend([
            "-P".to_string(),
            "-F".to_string(),
            "#{session_name}".to_string(),
        ]);
        if let Some(program) = config.initial_program.as_ref() {
            for (key, value) in &program.environment {
                args.push("-e".to_string());
                args.push(format!("{key}={value}"));
            }
            args.push(tmux_shell_command(&program.program, &program.args));
        } else {
            let default_shell = default_shell_path().unwrap_or_else(|| "/bin/bash".to_string());
            args.push(default_shell);
        }
        let output = self.run_workspace_command(workspace, &args)?;
        let session_name = parse_tmux_identifier(&output.stdout, "session name")?;
        if session_name != workspace.session_name.as_str() {
            return Err(TmuxError::new(format!(
                "vendored tmux created unexpected session `{session_name}` instead of `{}`",
                workspace.session_name.as_str()
            )));
        }
        Ok(())
    }

    fn sync_workspace_metadata(
        &self,
        config: &WorkspaceInstanceConfig,
        workspace: &TmuxWorkspaceHandle,
    ) -> Result<(), TmuxError> {
        self.set_session_environment(
            workspace,
            WAITAGENT_WORKSPACE_DIR_ENV,
            &config.workspace_dir.display().to_string(),
        )?;
        self.set_session_environment(
            workspace,
            WAITAGENT_WORKSPACE_KEY_ENV,
            &config.workspace_key,
        )?;
        self.set_session_environment(
            workspace,
            WAITAGENT_SESSION_ROLE_ENV,
            config.session_role.as_str(),
        )?;
        self.set_session_environment(
            workspace,
            WAITAGENT_TRANSPORT_ENV,
            WAITAGENT_TRANSPORT_LOCAL_TMUX,
        )?;
        Ok(())
    }

    fn set_session_environment(
        &self,
        workspace: &TmuxWorkspaceHandle,
        key: &str,
        value: &str,
    ) -> Result<(), TmuxError> {
        let args = vec![
            "set-environment".to_string(),
            "-t".to_string(),
            workspace.session_name.as_str().to_string(),
            key.to_string(),
            value.to_string(),
        ];
        self.run_workspace_command(workspace, &args)?;
        Ok(())
    }

    fn attach_to_socket_session(
        &self,
        socket_name: &TmuxSocketName,
        session_name: &str,
    ) -> Result<(), TmuxError> {
        let args = vec![
            "attach-session".to_string(),
            "-t".to_string(),
            session_name.to_string(),
        ];
        self.command_runner().run_interactive(socket_name, &args)
    }

    fn detach_session_on_socket(
        &self,
        socket_name: &TmuxSocketName,
        session_name: &str,
    ) -> Result<(), TmuxError> {
        let args = vec![
            "detach-client".to_string(),
            "-s".to_string(),
            session_name.to_string(),
        ];
        self.run_on_socket(socket_name, &args)?;
        Ok(())
    }

    pub(crate) fn discover_waitagent_sockets(&self) -> Result<Vec<TmuxSocketName>, TmuxError> {
        let socket_dir = tmux_socket_dir();
        if !socket_dir.exists() {
            return Ok(Vec::new());
        }

        let mut sockets = Vec::new();
        let entries = fs::read_dir(&socket_dir).map_err(|error| {
            TmuxError::new(format!(
                "failed to read tmux socket directory {}: {error}",
                socket_dir.display()
            ))
        })?;
        for entry in entries {
            let entry = entry.map_err(|error| {
                TmuxError::new(format!(
                    "failed to read tmux socket directory entry in {}: {error}",
                    socket_dir.display()
                ))
            })?;
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with(WAITAGENT_SOCKET_PREFIX)
                && entry
                    .file_type()
                    .map(|file_type| file_type.is_socket())
                    .unwrap_or(false)
            {
                sockets.push(TmuxSocketName::new(name));
            }
        }
        sockets.sort_by(|left, right| left.as_str().cmp(right.as_str()));
        Ok(sockets)
    }

    pub fn list_waitagent_session_entries(
        &self,
    ) -> Result<Vec<WaitagentSessionListEntry>, TmuxError> {
        let mut entries = Vec::new();
        for socket_name in self.discover_waitagent_sockets()? {
            entries.extend(self.list_session_entries_on_socket(&socket_name)?);
        }
        entries.sort_by(|left, right| {
            left.socket_name
                .cmp(&right.socket_name)
                .then_with(|| left.session_name.cmp(&right.session_name))
        });
        Ok(entries)
    }

    pub fn cleanup_stale_waitagent_socket_files(
        &self,
    ) -> Result<WaitagentSocketCleanupReport, TmuxError> {
        let mut report = WaitagentSocketCleanupReport::default();
        for socket_name in self.discover_waitagent_sockets()? {
            if self.socket_is_live(&socket_name) {
                report.live += 1;
                continue;
            }
            if remove_waitagent_socket_file(&socket_name)? {
                report.removed += 1;
            }
        }
        Ok(report)
    }

    pub fn remove_waitagent_socket_file(
        &self,
        socket_name: &TmuxSocketName,
    ) -> Result<bool, TmuxError> {
        remove_waitagent_socket_file(socket_name)
    }

    fn list_session_entries_on_socket(
        &self,
        socket_name: &TmuxSocketName,
    ) -> Result<Vec<WaitagentSessionListEntry>, TmuxError> {
        let args = vec![
            "list-sessions".to_string(),
            "-F".to_string(),
            "#{session_name}\t#{session_attached}\t#{session_windows}\t#{session_created}"
                .to_string(),
        ];
        let output = match self.run_on_socket(socket_name, &args) {
            Ok(output) => output,
            Err(error) if error.is_command_failure() => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };

        let mut entries = Vec::new();
        for line in output.stdout.lines() {
            let mut parts = line.split('\t');
            let Some(session_name) = parts.next() else {
                continue;
            };
            let Some(attached_clients) = parts.next() else {
                continue;
            };
            let Some(window_count) = parts.next() else {
                continue;
            };
            let created_at_unix_secs = parts.next().and_then(|value| value.parse::<u64>().ok());
            let metadata = match self.session_metadata(socket_name, session_name) {
                Ok(metadata) => metadata,
                Err(error) if error.is_command_failure() => return Ok(Vec::new()),
                Err(error) => return Err(error),
            };
            entries.push(WaitagentSessionListEntry {
                socket_name: socket_name.as_str().to_string(),
                session_name: session_name.to_string(),
                attached_clients: attached_clients.parse::<usize>().unwrap_or(0),
                window_count: window_count.parse::<usize>().unwrap_or(1),
                created_at_unix_secs,
                session_role: metadata.session_role,
            });
        }

        Ok(entries)
    }

    fn list_sessions_on_socket(
        &self,
        socket_name: &TmuxSocketName,
    ) -> Result<Vec<ManagedSessionRecord>, TmuxError> {
        let args = vec![
            "list-sessions".to_string(),
            "-F".to_string(),
            "#{session_name}\t#{session_attached}\t#{session_windows}".to_string(),
        ];
        let output = match self.run_on_socket(socket_name, &args) {
            Ok(output) => output,
            Err(error) if error.is_command_failure() => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };

        let mut records = Vec::new();
        for line in output.stdout.lines() {
            let mut parts = line.split('\t');
            let Some(session_name) = parts.next() else {
                continue;
            };
            let Some(attached_clients) = parts.next() else {
                continue;
            };
            let Some(window_count) = parts.next() else {
                continue;
            };
            let metadata = match self.session_metadata(socket_name, session_name) {
                Ok(metadata) => metadata,
                Err(error) if error.is_command_failure() => return Ok(Vec::new()),
                Err(error) => return Err(error),
            };
            let runtime = match self.session_runtime_metadata(socket_name, session_name) {
                Ok(runtime) => runtime,
                Err(error) if error.is_command_failure() => return Ok(Vec::new()),
                Err(error) => return Err(error),
            };
            records.push(ManagedSessionRecord {
                address: ManagedSessionAddress::local_tmux(
                    socket_name.as_str(),
                    session_name.to_string(),
                ),
                selector: Some(format!("{}:{}", socket_name.as_str(), session_name)),
                availability: if runtime.is_dead {
                    crate::domain::session_catalog::SessionAvailability::Exited
                } else {
                    crate::domain::session_catalog::SessionAvailability::Online
                },
                workspace_dir: metadata.workspace_dir,
                workspace_key: metadata.workspace_key,
                session_role: metadata.session_role,
                opened_by: Vec::new(),
                attached_clients: attached_clients.parse::<usize>().unwrap_or(0),
                window_count: window_count.parse::<usize>().unwrap_or(1),
                command_name: runtime.command_name,
                current_path: runtime.current_path,
                task_state: runtime.task_state,
            });
        }

        Ok(records)
    }

    fn session_metadata(
        &self,
        socket_name: &TmuxSocketName,
        session_name: &str,
    ) -> Result<TmuxSessionMetadata, TmuxError> {
        let args = vec![
            "show-environment".to_string(),
            "-t".to_string(),
            session_name.to_string(),
        ];
        let output = self.run_on_socket(socket_name, &args)?;
        let mut metadata = TmuxSessionMetadata::default();

        for line in output.stdout.lines() {
            if let Some((key, value)) = line.split_once('=') {
                match key {
                    WAITAGENT_WORKSPACE_DIR_ENV => {
                        metadata.workspace_dir = Some(PathBuf::from(value));
                    }
                    WAITAGENT_WORKSPACE_KEY_ENV => {
                        metadata.workspace_key = Some(value.to_string());
                    }
                    WAITAGENT_SESSION_ROLE_ENV => {
                        metadata.session_role = WorkspaceSessionRole::parse(value);
                    }
                    WAITAGENT_REMOTE_PUBLICATION_AUTHORITY_ID_ENV => {
                        metadata.remote_publication_authority_id = Some(value.to_string());
                    }
                    WAITAGENT_REMOTE_PUBLICATION_TRANSPORT_SESSION_ID_ENV => {
                        metadata.remote_publication_transport_session_id = Some(value.to_string());
                    }
                    WAITAGENT_REMOTE_PUBLICATION_SELECTOR_ENV => {
                        metadata.remote_publication_selector = Some(value.to_string());
                    }
                    _ => {}
                }
            }
        }

        Ok(metadata)
    }

    pub fn pane_dimensions_on_socket(
        &self,
        socket_name: &str,
        pane_target: &str,
    ) -> Result<(usize, usize), TmuxError> {
        let args = vec![
            "display-message".to_string(),
            "-p".to_string(),
            "-t".to_string(),
            pane_target.to_string(),
            "#{pane_width}\t#{pane_height}".to_string(),
        ];
        let output = self.run_on_socket(&TmuxSocketName::new(socket_name), &args)?;
        let mut parts = output.stdout.trim().split('\t');
        let width = parts.next().unwrap_or("0").parse::<usize>().unwrap_or(0);
        let height = parts.next().unwrap_or("0").parse::<usize>().unwrap_or(0);
        Ok((width, height))
    }

    pub(crate) fn wait_for_chrome_refresh_on_socket(
        &self,
        socket_name: &str,
        session_name: &str,
    ) -> Result<(), TmuxError> {
        wait_for_reliable_chrome_refresh(socket_name, session_name)
    }

    pub(crate) fn signal_chrome_refresh_on_socket(
        &self,
        socket_name: &str,
        session_name: &str,
    ) -> Result<(), TmuxError> {
        signal_reliable_chrome_refresh(socket_name, session_name)
    }

    pub(crate) fn run_chrome_refresh_owner_on_socket(
        &self,
        socket_name: &str,
        session_name: &str,
    ) -> Result<(), TmuxError> {
        let socket_path = chrome_refresh_owner_socket_path(socket_name, session_name);
        let listener = match bind_chrome_refresh_owner_socket(&socket_path) {
            Ok(listener) => listener,
            Err(error) if error.kind() == io::ErrorKind::AddrInUse => return Ok(()),
            Err(error) => return Err(chrome_refresh_tmux_error(error)),
        };
        run_chrome_refresh_owner(listener, socket_path);
        Ok(())
    }

    pub(crate) fn wait_for_sidebar_ready_on_socket(
        &self,
        socket_name: &str,
        session_name: &str,
    ) -> Result<(), TmuxError> {
        self.wait_for_workspace_channel_on_socket(
            socket_name,
            workspace_sidebar_ready_channel(session_name),
        )
    }

    pub(crate) fn signal_sidebar_ready_on_socket(
        &self,
        socket_name: &str,
        session_name: &str,
    ) -> Result<(), TmuxError> {
        self.signal_workspace_channel_on_socket(
            socket_name,
            workspace_sidebar_ready_channel(session_name),
        )
    }

    pub(crate) fn wait_for_footer_ready_on_socket(
        &self,
        socket_name: &str,
        session_name: &str,
    ) -> Result<(), TmuxError> {
        self.wait_for_workspace_channel_on_socket(
            socket_name,
            workspace_footer_ready_channel(session_name),
        )
    }

    pub(crate) fn signal_footer_ready_on_socket(
        &self,
        socket_name: &str,
        session_name: &str,
    ) -> Result<(), TmuxError> {
        self.signal_workspace_channel_on_socket(
            socket_name,
            workspace_footer_ready_channel(session_name),
        )
    }

    pub(crate) fn mark_sidebar_ready(
        &self,
        workspace: &TmuxWorkspaceHandle,
        pane_target: &str,
    ) -> Result<(), TmuxError> {
        self.set_session_option(workspace, WAITAGENT_SIDEBAR_READY_OPTION, pane_target)?;
        self.signal_sidebar_ready_on_socket(
            workspace.socket_name.as_str(),
            workspace.session_name.as_str(),
        )
    }

    pub(crate) fn mark_footer_ready(
        &self,
        workspace: &TmuxWorkspaceHandle,
        pane_target: &str,
    ) -> Result<(), TmuxError> {
        self.set_session_option(workspace, WAITAGENT_FOOTER_READY_OPTION, pane_target)?;
        self.signal_footer_ready_on_socket(
            workspace.socket_name.as_str(),
            workspace.session_name.as_str(),
        )
    }

    pub(crate) fn sidebar_ready_matches(
        &self,
        workspace: &TmuxWorkspaceHandle,
        pane_target: &str,
    ) -> Result<bool, TmuxError> {
        self.show_session_option(workspace, WAITAGENT_SIDEBAR_READY_OPTION)
            .map(|value| value.as_deref() == Some(pane_target))
    }

    pub(crate) fn footer_ready_matches(
        &self,
        workspace: &TmuxWorkspaceHandle,
        pane_target: &str,
    ) -> Result<bool, TmuxError> {
        self.show_session_option(workspace, WAITAGENT_FOOTER_READY_OPTION)
            .map(|value| value.as_deref() == Some(pane_target))
    }

    #[allow(dead_code)]
    pub fn capture_pane_text_on_socket(
        &self,
        socket_name: &str,
        pane_target: &str,
    ) -> Result<String, TmuxError> {
        let args = vec![
            "capture-pane".to_string(),
            "-p".to_string(),
            "-t".to_string(),
            pane_target.to_string(),
        ];
        let output = self.run_on_socket(&TmuxSocketName::new(socket_name), &args)?;
        Ok(output.stdout)
    }

    /// Captures the full pane history from the earliest available scrollback
    /// to the current content. When the pane is in alt-screen mode, the normal
    /// (pre-alt-screen) buffer is captured; use `capture_pane_alt_history_on_socket`
    /// for the alt-screen buffer.
    #[allow(dead_code)]
    pub fn capture_pane_full_history_on_socket(
        &self,
        socket_name: &str,
        pane_target: &str,
    ) -> Result<String, TmuxError> {
        let args = vec![
            "capture-pane".to_string(),
            "-p".to_string(),
            "-S".to_string(),
            "-".to_string(),
            "-t".to_string(),
            pane_target.to_string(),
        ];
        let output = self.run_on_socket(&TmuxSocketName::new(socket_name), &args)?;
        Ok(output.stdout)
    }

    pub fn capture_pane_ansi_on_socket(
        &self,
        socket_name: &str,
        pane_target: &str,
    ) -> Result<String, TmuxError> {
        let args = vec![
            "capture-pane".to_string(),
            "-p".to_string(),
            "-e".to_string(),
            "-t".to_string(),
            pane_target.to_string(),
        ];
        let output = self.run_on_socket(&TmuxSocketName::new(socket_name), &args)?;
        Ok(output.stdout)
    }

    /// Captures only the visible portion of the pane (no scrollback history).
    /// Used for reconnect catch-up when the server already has output_log history
    /// but needs the current terminal state after a disconnect.
    pub fn capture_pane_ansi_visible_on_socket(
        &self,
        socket_name: &str,
        pane_target: &str,
    ) -> Result<String, TmuxError> {
        let args = vec![
            "capture-pane".to_string(),
            "-p".to_string(),
            "-e".to_string(),
            "-t".to_string(),
            pane_target.to_string(),
        ];
        let output = self.run_on_socket(&TmuxSocketName::new(socket_name), &args)?;
        Ok(output.stdout)
    }

    pub fn pane_cursor_position_on_socket(
        &self,
        socket_name: &str,
        pane_target: &str,
    ) -> Result<(usize, usize), TmuxError> {
        let args = vec![
            "display-message".to_string(),
            "-p".to_string(),
            "-t".to_string(),
            pane_target.to_string(),
            "#{cursor_x}\t#{cursor_y}".to_string(),
        ];
        let output = self.run_on_socket(&TmuxSocketName::new(socket_name), &args)?;
        let mut parts = output.stdout.trim().split('\t');
        let cursor_x = parts.next().unwrap_or("0").parse::<usize>().unwrap_or(0);
        let cursor_y = parts.next().unwrap_or("0").parse::<usize>().unwrap_or(0);
        Ok((cursor_x, cursor_y))
    }

    pub fn pane_terminal_flags_on_socket(
        &self,
        socket_name: &str,
        pane_target: &str,
    ) -> Result<RemoteTargetTerminalFlags, TmuxError> {
        let args = vec![
            "display-message".to_string(),
            "-p".to_string(),
            "-t".to_string(),
            pane_target.to_string(),
            "#{alternate_on}\t#{keypad_cursor_flag}\t#{cursor_flag}".to_string(),
        ];
        let output = self.run_on_socket(&TmuxSocketName::new(socket_name), &args)?;
        let mut parts = output.stdout.trim().split('\t');
        Ok(RemoteTargetTerminalFlags {
            alternate_screen_active: parts.next().unwrap_or("0") == "1",
            application_cursor_keys: parts.next().unwrap_or("0") == "1",
            cursor_visible: parts.next().unwrap_or("1") == "1",
        })
    }

    fn wait_for_workspace_channel_on_socket(
        &self,
        socket_name: &str,
        channel_name: String,
    ) -> Result<(), TmuxError> {
        self.run_on_socket(
            &TmuxSocketName::new(socket_name),
            &["wait-for".to_string(), channel_name],
        )
        .map(|_| ())
    }

    fn signal_workspace_channel_on_socket(
        &self,
        socket_name: &str,
        channel_name: String,
    ) -> Result<(), TmuxError> {
        self.run_on_socket(
            &TmuxSocketName::new(socket_name),
            &["wait-for".to_string(), "-S".to_string(), channel_name],
        )
        .map(|_| ())
    }

    pub(crate) fn pane_in_mode_on_socket(
        &self,
        socket_name: &str,
        pane_target: &str,
    ) -> Result<bool, TmuxError> {
        let args = vec![
            "display-message".to_string(),
            "-p".to_string(),
            "-t".to_string(),
            pane_target.to_string(),
            "#{pane_in_mode}".to_string(),
        ];
        let output = self.run_on_socket(&TmuxSocketName::new(socket_name), &args)?;
        Ok(output.stdout.trim() == "1")
    }

    pub(crate) fn cancel_pane_mode_on_socket(
        &self,
        socket_name: &str,
        pane_target: &str,
    ) -> Result<(), TmuxError> {
        self.run_on_socket(
            &TmuxSocketName::new(socket_name),
            &[
                "send-keys".to_string(),
                "-X".to_string(),
                "-t".to_string(),
                pane_target.to_string(),
                "cancel".to_string(),
            ],
        )
        .map(|_| ())
    }

    #[allow(dead_code)]
    pub(crate) fn send_keys_copy_mode_on_socket(
        &self,
        socket_name: &str,
        pane_target: &str,
        key: &str,
    ) -> Result<(), TmuxError> {
        self.run_on_socket(
            &TmuxSocketName::new(socket_name),
            &[
                "send-keys".to_string(),
                "-X".to_string(),
                "-t".to_string(),
                pane_target.to_string(),
                key.to_string(),
            ],
        )
        .map(|_| ())
    }

    pub fn window_zoomed_on_socket(
        &self,
        socket_name: &str,
        pane_target: &str,
    ) -> Result<bool, TmuxError> {
        let args = vec![
            "display-message".to_string(),
            "-p".to_string(),
            "-t".to_string(),
            pane_target.to_string(),
            "#{window_zoomed_flag}".to_string(),
        ];
        let output = self.run_on_socket(&TmuxSocketName::new(socket_name), &args)?;
        Ok(output.stdout.trim() == "1")
    }

    fn find_managed_session(
        &self,
        target: &str,
    ) -> Result<Option<ManagedSessionRecord>, TmuxError> {
        let mut matches = self
            .list_sessions()?
            .into_iter()
            .filter(|session| session.matches_target(target))
            .collect::<Vec<_>>();

        if matches.len() > 1 {
            return Err(TmuxError::new(format!(
                "ambiguous waitagent tmux target `{target}`; use socket:session"
            )));
        }

        Ok(matches.pop())
    }

    fn tmux_program_args(&self, base_args: &[String], program: &TmuxProgram) -> Vec<String> {
        let mut args = base_args.to_vec();
        if let Some(start_directory) = program.start_directory.as_ref() {
            args.push("-c".to_string());
            args.push(start_directory.display().to_string());
        }
        for (key, value) in &program.environment {
            args.push("-e".to_string());
            args.push(format!("{key}={value}"));
        }
        args.push(program.program.clone());
        args.extend(program.args.iter().cloned());
        args
    }
}

pub(crate) fn exact_session_target(session_name: &str) -> String {
    format!("={session_name}:")
}

pub(crate) fn waitagent_socket_path(socket_name: &TmuxSocketName) -> Result<PathBuf, TmuxError> {
    if !socket_name.as_str().starts_with(WAITAGENT_SOCKET_PREFIX) {
        return Err(TmuxError::new(format!(
            "refusing to manage non-waitagent tmux socket `{}`",
            socket_name.as_str()
        )));
    }
    Ok(tmux_socket_dir().join(socket_name.as_str()))
}

pub(crate) fn remove_waitagent_socket_file(
    socket_name: &TmuxSocketName,
) -> Result<bool, TmuxError> {
    let socket_path = waitagent_socket_path(socket_name)?;
    match fs::remove_file(&socket_path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(TmuxError::new(format!(
            "failed to remove stale waitagent tmux socket {}: {error}",
            socket_path.display()
        ))),
    }
}

fn tmux_shell_command(program: &str, program_args: &[String]) -> String {
    let mut parts = vec![shell_escape(program)];
    parts.extend(program_args.iter().map(|arg| shell_escape(arg)));
    parts.join(" ")
}

fn shell_escape(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn wait_for_reliable_chrome_refresh(
    socket_name: &str,
    session_name: &str,
) -> Result<(), TmuxError> {
    let socket_path = chrome_refresh_owner_socket_path(socket_name, session_name);
    for attempt in 0..2 {
        let result = CHROME_REFRESH_SUBSCRIBERS.with(|subscribers| {
            let mut subscribers = subscribers.borrow_mut();
            if !subscribers.contains_key(&socket_path) {
                let stream =
                    connect_chrome_refresh_subscriber(socket_name, session_name, &socket_path)?;
                subscribers.insert(socket_path.clone(), stream);
            }
            let stream = subscribers
                .get_mut(&socket_path)
                .expect("chrome refresh subscriber should be inserted");
            let mut event = [0u8; 1];
            stream
                .read_exact(&mut event)
                .map_err(chrome_refresh_tmux_error)
        });
        if result.is_ok() {
            return Ok(());
        }
        CHROME_REFRESH_SUBSCRIBERS.with(|subscribers| {
            subscribers.borrow_mut().remove(&socket_path);
        });
        if attempt == 1 {
            return result;
        }
    }
    unreachable!("chrome refresh wait retry loop always returns");
}

fn signal_reliable_chrome_refresh(socket_name: &str, session_name: &str) -> Result<(), TmuxError> {
    let socket_path = chrome_refresh_owner_socket_path(socket_name, session_name);
    let Some(mut stream) = connect_chrome_refresh_owner_if_available(&socket_path)? else {
        return Ok(());
    };
    writeln!(stream, "SIGNAL").map_err(chrome_refresh_tmux_error)?;
    stream.flush().map_err(chrome_refresh_tmux_error)?;
    read_chrome_refresh_ok(&mut stream)
}

fn connect_chrome_refresh_subscriber(
    socket_name: &str,
    session_name: &str,
    socket_path: &Path,
) -> Result<UnixStream, TmuxError> {
    ensure_chrome_refresh_owner(socket_name, session_name, socket_path)?;
    let mut stream = UnixStream::connect(socket_path).map_err(|error| {
        TmuxError::new(format!(
            "failed to connect chrome refresh owner `{}`: {error}",
            socket_path.display()
        ))
    })?;
    writeln!(stream, "SUBSCRIBE").map_err(chrome_refresh_tmux_error)?;
    stream.flush().map_err(chrome_refresh_tmux_error)?;
    read_chrome_refresh_ok(&mut stream)?;
    Ok(stream)
}

fn connect_chrome_refresh_owner_if_available(
    socket_path: &Path,
) -> Result<Option<UnixStream>, TmuxError> {
    match chrome_refresh_owner_status(socket_path) {
        ChromeRefreshOwnerStatus::Ready => UnixStream::connect(socket_path)
            .map(Some)
            .map_err(chrome_refresh_tmux_error),
        ChromeRefreshOwnerStatus::Starting => {
            if wait_for_chrome_refresh_owner(socket_path).is_err() {
                return Ok(None);
            }
            UnixStream::connect(socket_path)
                .map(Some)
                .map_err(chrome_refresh_tmux_error)
        }
        ChromeRefreshOwnerStatus::Stale => {
            let _ = fs::remove_file(socket_path);
            Ok(None)
        }
    }
}

fn read_chrome_refresh_ok(stream: &mut UnixStream) -> Result<(), TmuxError> {
    let response = read_chrome_refresh_line(stream).map_err(chrome_refresh_tmux_error)?;
    if response.trim() == "OK" {
        Ok(())
    } else {
        Err(TmuxError::new(format!(
            "chrome refresh owner returned unexpected response `{}`",
            response.trim()
        )))
    }
}

fn read_chrome_refresh_line(stream: &mut UnixStream) -> io::Result<String> {
    let mut response = Vec::new();
    let mut byte = [0u8; 1];
    while response.len() < 128 {
        let read = stream.read(&mut byte)?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "chrome refresh owner closed the connection",
            ));
        }
        if byte[0] == b'\n' {
            return String::from_utf8(response)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.utf8_error()));
        }
        response.push(byte[0]);
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "chrome refresh owner response is too long",
    ))
}

fn ensure_chrome_refresh_owner(
    _socket_name: &str,
    _session_name: &str,
    socket_path: &Path,
) -> Result<(), TmuxError> {
    match chrome_refresh_owner_status(socket_path) {
        ChromeRefreshOwnerStatus::Ready => return Ok(()),
        ChromeRefreshOwnerStatus::Starting => {
            if wait_for_chrome_refresh_owner(socket_path).is_ok() {
                return Ok(());
            }
        }
        ChromeRefreshOwnerStatus::Stale => {}
    }
    #[cfg(not(test))]
    {
        return spawn_chrome_refresh_owner_process(_socket_name, _session_name, socket_path);
    }
    #[cfg(test)]
    match bind_chrome_refresh_owner_socket(socket_path) {
        Ok(listener) => {
            let socket_path = socket_path.to_path_buf();
            thread::Builder::new()
                .name("chrome-refresh-owner".to_string())
                .spawn(move || run_chrome_refresh_owner(listener, socket_path))
                .map_err(chrome_refresh_tmux_error)?;
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::AddrInUse => {
            wait_for_chrome_refresh_owner(socket_path)
        }
        Err(error) => Err(chrome_refresh_tmux_error(error)),
    }
}

fn bind_chrome_refresh_owner_socket(socket_path: &Path) -> io::Result<UnixListener> {
    match UnixListener::bind(socket_path) {
        Ok(listener) => return Ok(listener),
        Err(error) if error.kind() == io::ErrorKind::AddrInUse => {}
        Err(error) => return Err(error),
    }
    match chrome_refresh_owner_status(socket_path) {
        ChromeRefreshOwnerStatus::Ready => Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            "chrome refresh owner is already running",
        )),
        ChromeRefreshOwnerStatus::Starting => Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            "chrome refresh owner is starting",
        )),
        ChromeRefreshOwnerStatus::Stale => {
            let _ = fs::remove_file(socket_path);
            UnixListener::bind(socket_path)
        }
    }
}

#[cfg(not(test))]
fn spawn_chrome_refresh_owner_process(
    socket_name: &str,
    session_name: &str,
    socket_path: &Path,
) -> Result<(), TmuxError> {
    let current_exe = std::env::current_exe().map_err(chrome_refresh_tmux_error)?;
    Command::new(current_exe)
        .args([
            "__chrome-refresh-owner",
            "--socket-name",
            socket_name,
            "--session-name",
            session_name,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(chrome_refresh_tmux_error)?;
    wait_for_chrome_refresh_owner(socket_path)
}

fn run_chrome_refresh_owner(listener: UnixListener, socket_path: PathBuf) {
    if listener.set_nonblocking(true).is_err() {
        let _ = fs::remove_file(socket_path);
        return;
    }
    let mut subscribers: Vec<UnixStream> = Vec::new();
    let mut had_subscriber = false;
    loop {
        if had_subscriber && subscribers.is_empty() {
            break;
        }

        let mut poll_fds = Vec::with_capacity(subscribers.len() + 1);
        poll_fds.push(libc::pollfd {
            fd: listener.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        });
        for subscriber in &subscribers {
            poll_fds.push(libc::pollfd {
                fd: subscriber.as_raw_fd(),
                events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
                revents: 0,
            });
        }

        let poll_result =
            unsafe { libc::poll(poll_fds.as_mut_ptr(), poll_fds.len() as libc::nfds_t, -1) };
        if poll_result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }

        let mut dead_subscribers = Vec::new();
        for (index, poll_fd) in poll_fds.iter().enumerate().skip(1) {
            if subscriber_disconnected(poll_fd.revents, &mut subscribers[index - 1]) {
                dead_subscribers.push(index - 1);
            }
        }
        for index in dead_subscribers.into_iter().rev() {
            subscribers.remove(index);
        }

        if poll_fds[0].revents & libc::POLLIN == 0 {
            continue;
        }

        loop {
            let mut stream = match listener.accept() {
                Ok((stream, _)) => stream,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(_) => {
                    let _ = fs::remove_file(socket_path);
                    return;
                }
            };
            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
            let request = match read_chrome_refresh_line(&mut stream) {
                Ok(request) => request,
                Err(_) => continue,
            };
            let request = request.trim();
            if request == "PING" {
                let _ = writeln!(stream, "OK");
            } else if request == "SIGNAL" {
                let _ = writeln!(stream, "OK");
                let mut live_subscribers = Vec::with_capacity(subscribers.len());
                for mut subscriber in subscribers.drain(..) {
                    if subscriber.write_all(b"R").is_ok() && subscriber.flush().is_ok() {
                        live_subscribers.push(subscriber);
                    }
                }
                subscribers = live_subscribers;
            } else if request == "SUBSCRIBE" {
                if writeln!(stream, "OK").is_ok() && stream.flush().is_ok() {
                    let _ = stream.set_read_timeout(None);
                    subscribers.push(stream);
                    had_subscriber = true;
                }
            } else {
                let _ = writeln!(stream, "ERR unknown-command");
            }
        }
    }
    let _ = fs::remove_file(socket_path);
}

fn subscriber_disconnected(revents: i16, subscriber: &mut UnixStream) -> bool {
    if revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
        return true;
    }
    if revents & libc::POLLIN == 0 {
        return false;
    }
    let mut buf = [0u8; 1];
    let received = unsafe {
        libc::recv(
            subscriber.as_raw_fd(),
            buf.as_mut_ptr().cast(),
            buf.len(),
            libc::MSG_PEEK | libc::MSG_DONTWAIT,
        )
    };
    if received == 0 {
        return true;
    }
    if received > 0 {
        return false;
    }
    let error = io::Error::last_os_error();
    error.kind() != io::ErrorKind::WouldBlock
}

fn chrome_refresh_owner_available(socket_path: &Path) -> bool {
    matches!(
        chrome_refresh_owner_status(socket_path),
        ChromeRefreshOwnerStatus::Ready
    )
}

enum ChromeRefreshOwnerStatus {
    Ready,
    Starting,
    Stale,
}

fn chrome_refresh_owner_status(socket_path: &Path) -> ChromeRefreshOwnerStatus {
    let Ok(mut stream) = UnixStream::connect(socket_path) else {
        return ChromeRefreshOwnerStatus::Stale;
    };
    if writeln!(stream, "PING").is_err() || stream.flush().is_err() {
        return ChromeRefreshOwnerStatus::Stale;
    }
    let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
    if read_chrome_refresh_ok(&mut stream).is_ok() {
        ChromeRefreshOwnerStatus::Ready
    } else {
        ChromeRefreshOwnerStatus::Starting
    }
}

fn wait_for_chrome_refresh_owner(socket_path: &Path) -> Result<(), TmuxError> {
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while std::time::Instant::now() < deadline {
        if chrome_refresh_owner_available(socket_path) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(10));
    }
    Err(TmuxError::new(format!(
        "chrome refresh owner `{}` did not become ready",
        socket_path.display()
    )))
}

fn chrome_refresh_owner_socket_path(socket_name: &str, session_name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "waitagent-chrome-refresh-{}.sock",
        stable_socket_hash(&[socket_name, session_name])
    ))
}

fn stable_socket_hash(values: &[&str]) -> String {
    let mut hash = 0xcbf29ce484222325_u64;
    for value in values {
        for byte in value.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
    }
    format!("{hash:016x}")
}

fn chrome_refresh_tmux_error(
    error: impl Into<Box<dyn std::error::Error + Send + Sync>>,
) -> TmuxError {
    TmuxError::new(format!(
        "chrome refresh signal channel failed: {}",
        error.into()
    ))
}

pub(crate) mod process_inspector;
pub(crate) use process_inspector::foreground_process_argvs_for_pane_shell;

pub(crate) use crate::infra::tmux_error::parse_tmux_id;

mod gateway;
use gateway::{
    default_shell_path, default_window_name, workspace_footer_ready_channel,
    workspace_sidebar_ready_channel,
};

#[cfg(test)]
mod tmux_backend_test;
