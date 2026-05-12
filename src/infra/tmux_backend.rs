use crate::domain::agent_detector::DetectorRegistry;
use crate::domain::session_catalog::{
    ManagedSessionAddress, ManagedSessionRecord, ManagedSessionTaskState,
};
use crate::domain::workspace::{
    WorkspaceInstanceConfig, WorkspaceInstanceId, WorkspaceSessionRole,
};
use crate::infra::tmux_error::{
    parse_tmux_id, parse_tmux_identifier, tmux_socket_dir, validate_percent, TmuxCommandOutput,
    TmuxCommandRunner, TmuxError,
};
use crate::infra::tmux_glue::{
    TmuxGlueArtifacts, TmuxGlueBuildConfig, TmuxGlueBuildStatus, VendoredTmuxSource,
};
use crate::infra::tmux_types::{
    TmuxLayoutGateway, TmuxPaneId, TmuxPaneInfo, TmuxProgram, TmuxSessionGateway, TmuxSessionName,
    TmuxSocketName, TmuxWorkspaceHandle,
};
use crate::runtime::remote_authority_target_host_runtime::RemoteTargetTerminalFlags;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

mod control;
mod layout;
mod remote;

const WAITAGENT_SOCKET_PREFIX: &str = "wa-";
const SYSTEM_TMUX_PROGRAM: &str = "tmux";
const WAITAGENT_WORKSPACE_DIR_ENV: &str = "WAITAGENT_WORKSPACE_DIR";
const WAITAGENT_WORKSPACE_KEY_ENV: &str = "WAITAGENT_WORKSPACE_KEY";
const WAITAGENT_SESSION_ROLE_ENV: &str = "WAITAGENT_SESSION_ROLE";
const WAITAGENT_TRANSPORT_ENV: &str = "WAITAGENT_SESSION_TRANSPORT";
const WAITAGENT_TRANSPORT_LOCAL_TMUX: &str = "local-tmux";
pub(crate) const WAITAGENT_REMOTE_PUBLICATION_AUTHORITY_ID_ENV: &str =
    "WAITAGENT_REMOTE_PUBLICATION_AUTHORITY_ID";
pub(crate) const WAITAGENT_REMOTE_PUBLICATION_TRANSPORT_SESSION_ID_ENV: &str =
    "WAITAGENT_REMOTE_PUBLICATION_TRANSPORT_SESSION_ID";
pub(crate) const WAITAGENT_REMOTE_PUBLICATION_SELECTOR_ENV: &str =
    "WAITAGENT_REMOTE_PUBLICATION_SELECTOR";
const WAITAGENT_SIDEBAR_PANE_TITLE: &str = "waitagent-sidebar";
const WAITAGENT_FOOTER_PANE_TITLE: &str = "waitagent-footer";
const WAITAGENT_CHROME_REFRESH_CHANNEL_PREFIX: &str = "waitagent-chrome-refresh";
const WAITAGENT_SIDEBAR_READY_CHANNEL_PREFIX: &str = "waitagent-sidebar-ready";
const WAITAGENT_FOOTER_READY_CHANNEL_PREFIX: &str = "waitagent-footer-ready";
const WAITAGENT_SIDEBAR_READY_OPTION: &str = "@waitagent_sidebar_ready_pane";
const WAITAGENT_FOOTER_READY_OPTION: &str = "@waitagent_footer_ready_pane";
const DEFAULT_HISTORY_LIMIT: &str = "100000";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddedTmuxBackend {
    source: VendoredTmuxSource,
    artifacts: TmuxGlueArtifacts,
    build_status: TmuxGlueBuildStatus,
    build_config: TmuxGlueBuildConfig,
    registry: Arc<DetectorRegistry>,
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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct TmuxSessionRuntimeMetadata {
    command_name: Option<String>,
    current_path: Option<PathBuf>,
    task_state: ManagedSessionTaskState,
    is_dead: bool,
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

    fn run_on_socket(
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
                workspace.session_name.as_str().to_string(),
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
            session_name.to_string(),
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
                workspace.session_name.as_str().to_string(),
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
                workspace.session_name.as_str().to_string(),
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
        let default_shell = default_shell_path().unwrap_or_else(|| "/bin/bash".to_string());
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
        args.push(default_shell);
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
            if name.starts_with(WAITAGENT_SOCKET_PREFIX) {
                sockets.push(TmuxSocketName::new(name));
            }
        }
        sockets.sort_by(|left, right| left.as_str().cmp(right.as_str()));
        Ok(sockets)
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

    fn session_runtime_metadata(
        &self,
        socket_name: &TmuxSocketName,
        session_name: &str,
    ) -> Result<TmuxSessionRuntimeMetadata, TmuxError> {
        let panes = self.list_panes_on_target(socket_name, session_name)?;
        let Some(main_pane) = panes.iter().find(|pane| {
            pane.title != WAITAGENT_SIDEBAR_PANE_TITLE && pane.title != WAITAGENT_FOOTER_PANE_TITLE
        }) else {
            return Ok(TmuxSessionRuntimeMetadata::default());
        };
        let pane_text = self.capture_pane_text(socket_name, &main_pane.pane_id)?;
        let current_command = main_pane.current_command.as_deref().unwrap_or_default();
        let foreground_argv = foreground_process_argv_for_pane_shell(main_pane.pane_pid);
        let command_name = self.registry.detect_command_name(
            current_command,
            foreground_argv.as_deref(),
            &pane_text,
        );
        let task_state = if main_pane.in_mode {
            ManagedSessionTaskState::Running
        } else {
            self.registry
                .infer_task_state(Some(&command_name), &pane_text)
        };
        Ok(TmuxSessionRuntimeMetadata {
            command_name: Some(command_name.clone()),
            current_path: main_pane.current_path.clone(),
            task_state,
            is_dead: main_pane.is_dead,
        })
    }

    fn list_panes_on_target(
        &self,
        socket_name: &TmuxSocketName,
        target: &str,
    ) -> Result<Vec<TmuxPaneInfo>, TmuxError> {
        let args = vec![
            "list-panes".to_string(),
            "-t".to_string(),
            target.to_string(),
            "-F".to_string(),
            "#{pane_id}\t#{pane_pid}\t#{pane_title}\t#{pane_current_command}\t#{pane_current_path}\t#{pane_dead}\t#{pane_in_mode}"
                .to_string(),
        ];
        let output = self.run_on_socket(socket_name, &args)?;
        output
            .stdout
            .lines()
            .map(Self::pane_info_for_line)
            .collect::<Result<Vec<_>, _>>()
    }

    fn capture_pane_text(
        &self,
        socket_name: &TmuxSocketName,
        pane_id: &TmuxPaneId,
    ) -> Result<String, TmuxError> {
        let args = vec![
            "capture-pane".to_string(),
            "-p".to_string(),
            "-t".to_string(),
            pane_id.as_str().to_string(),
        ];
        let output = self.run_on_socket(socket_name, &args)?;
        Ok(output.stdout)
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
        self.run_on_socket(
            &TmuxSocketName::new(socket_name),
            &[
                "wait-for".to_string(),
                workspace_chrome_refresh_channel(session_name),
            ],
        )
        .map(|_| ())
    }

    pub(crate) fn signal_chrome_refresh_on_socket(
        &self,
        socket_name: &str,
        session_name: &str,
    ) -> Result<(), TmuxError> {
        self.run_on_socket(
            &TmuxSocketName::new(socket_name),
            &[
                "wait-for".to_string(),
                "-S".to_string(),
                workspace_chrome_refresh_channel(session_name),
            ],
        )
        .map(|_| ())
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

    pub fn capture_pane_ansi_on_socket(
        &self,
        socket_name: &str,
        pane_target: &str,
    ) -> Result<String, TmuxError> {
        let args = vec![
            "capture-pane".to_string(),
            "-p".to_string(),
            "-e".to_string(),
            "-N".to_string(),
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

    fn pane_info_for_line(line: &str) -> Result<TmuxPaneInfo, TmuxError> {
        let mut parts = line.splitn(7, '\t');
        let pane_id = parts.next().unwrap_or_default();
        let pane_pid = parts.next().unwrap_or_default();
        let title = parts.next().unwrap_or_default();
        let current_command = parts.next().unwrap_or_default();
        let current_path = parts.next().unwrap_or_default();
        let dead = parts.next().unwrap_or_default();
        let in_mode = parts.next().unwrap_or_default();

        Ok(TmuxPaneInfo {
            pane_id: TmuxPaneId::new(parse_tmux_id(pane_id, '%', "pane id")?),
            pane_pid: pane_pid.parse::<u32>().ok(),
            title: title.to_string(),
            current_command: (!current_command.is_empty()).then(|| current_command.to_string()),
            current_path: (!current_path.is_empty()).then(|| PathBuf::from(current_path)),
            is_dead: dead == "1",
            in_mode: in_mode == "1",
        })
    }
}

pub(crate) mod process_inspector;
pub(crate) use process_inspector::foreground_process_argv_for_pane_shell;

mod gateway;
use gateway::{
    default_shell_path, default_window_name, workspace_chrome_refresh_channel,
    workspace_footer_ready_channel, workspace_sidebar_ready_channel,
};

#[cfg(test)]
mod tmux_backend_test;
