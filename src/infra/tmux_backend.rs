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
    TmuxChromeGateway, TmuxGateway, TmuxPaneId, TmuxPaneInfo, TmuxProgram, TmuxSessionGateway,
    TmuxSessionName, TmuxSocketName, TmuxWindowHandle, TmuxWindowId, TmuxWorkspaceHandle,
};
use std::fs;
use std::path::{Path, PathBuf};

mod control;
mod layout;

const WAITAGENT_SOCKET_PREFIX: &str = "wa-";
const WAITAGENT_WORKSPACE_DIR_ENV: &str = "WAITAGENT_WORKSPACE_DIR";
const WAITAGENT_WORKSPACE_KEY_ENV: &str = "WAITAGENT_WORKSPACE_KEY";
const WAITAGENT_SESSION_ROLE_ENV: &str = "WAITAGENT_SESSION_ROLE";
const WAITAGENT_TRANSPORT_ENV: &str = "WAITAGENT_SESSION_TRANSPORT";
const WAITAGENT_TRANSPORT_LOCAL_TMUX: &str = "local-tmux";
const WAITAGENT_SIDEBAR_PANE_TITLE: &str = "waitagent-sidebar";
const WAITAGENT_FOOTER_PANE_TITLE: &str = "waitagent-footer";
const WAITAGENT_CHROME_REFRESH_CHANNEL_PREFIX: &str = "waitagent-chrome-refresh";
const DEFAULT_HISTORY_LIMIT: &str = "100000";
const TMUX_MOUSE_OPTION: &str = "mouse";
const TMUX_OPTION_ON: &str = "on";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddedTmuxBackend {
    source: VendoredTmuxSource,
    artifacts: TmuxGlueArtifacts,
    build_status: TmuxGlueBuildStatus,
    build_config: TmuxGlueBuildConfig,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct TmuxSessionMetadata {
    workspace_dir: Option<PathBuf>,
    workspace_key: Option<String>,
    session_role: Option<WorkspaceSessionRole>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct TmuxSessionRuntimeMetadata {
    command_name: Option<String>,
    current_path: Option<PathBuf>,
    task_state: ManagedSessionTaskState,
}

impl EmbeddedTmuxBackend {
    pub fn new(
        source: VendoredTmuxSource,
        artifacts: TmuxGlueArtifacts,
        build_status: TmuxGlueBuildStatus,
        build_config: TmuxGlueBuildConfig,
    ) -> Self {
        Self {
            source,
            artifacts,
            build_status,
            build_config,
        }
    }

    pub fn source(&self) -> &VendoredTmuxSource {
        &self.source
    }

    pub fn build_config(&self) -> &TmuxGlueBuildConfig {
        &self.build_config
    }

    pub fn artifacts(&self) -> &TmuxGlueArtifacts {
        &self.artifacts
    }

    pub fn build_status(&self) -> &TmuxGlueBuildStatus {
        &self.build_status
    }

    pub fn from_build_env() -> Result<Self, TmuxError> {
        let source = VendoredTmuxSource::discover_from_build_env()?;
        let artifacts = TmuxGlueArtifacts::from_build_env()?;
        let build_status = TmuxGlueBuildStatus::from_build_env()?;
        let build_config = TmuxGlueBuildConfig::from_artifacts(&artifacts);
        let backend = Self::new(source, artifacts, build_status, build_config);
        backend.validate_runtime_artifacts()?;
        Ok(backend)
    }

    fn validate_runtime_artifacts(&self) -> Result<(), TmuxError> {
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
            TMUX_MOUSE_OPTION.to_string(),
            TMUX_OPTION_ON.to_string(),
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

    fn discover_waitagent_sockets(&self) -> Result<Vec<TmuxSocketName>, TmuxError> {
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
                workspace_dir: metadata.workspace_dir,
                workspace_key: metadata.workspace_key,
                session_role: metadata.session_role,
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
        Ok(TmuxSessionRuntimeMetadata {
            command_name: main_pane.current_command.clone(),
            current_path: main_pane.current_path.clone(),
            task_state: ManagedSessionTaskState::infer(
                main_pane.current_command.as_deref(),
                &pane_text,
            ),
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
            "#{pane_id}\t#{pane_title}\t#{pane_current_command}\t#{pane_current_path}\t#{pane_dead}"
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
            "-S".to_string(),
            "-40".to_string(),
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
            "-S".to_string(),
            "-40".to_string(),
        ];
        let output = self.run_on_socket(&TmuxSocketName::new(socket_name), &args)?;
        Ok(output.stdout)
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
        let mut parts = line.splitn(5, '\t');
        let pane_id = parts.next().unwrap_or_default();
        let title = parts.next().unwrap_or_default();
        let current_command = parts.next().unwrap_or_default();
        let current_path = parts.next().unwrap_or_default();
        let dead = parts.next().unwrap_or_default();

        Ok(TmuxPaneInfo {
            pane_id: TmuxPaneId::new(parse_tmux_id(pane_id, '%', "pane id")?),
            title: title.to_string(),
            current_command: (!current_command.is_empty()).then(|| current_command.to_string()),
            current_path: (!current_path.is_empty()).then(|| PathBuf::from(current_path)),
            is_dead: dead == "1",
        })
    }
}

fn workspace_chrome_refresh_channel(session_name: &str) -> String {
    format!("{WAITAGENT_CHROME_REFRESH_CHANNEL_PREFIX}-{session_name}")
}

fn default_window_name() -> String {
    std::env::var("SHELL")
        .ok()
        .and_then(|value| {
            Path::new(&value)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "shell".to_string())
}

impl TmuxGateway for EmbeddedTmuxBackend {
    type Error = TmuxError;

    fn ensure_workspace(
        &self,
        config: &WorkspaceInstanceConfig,
    ) -> Result<TmuxWorkspaceHandle, Self::Error> {
        let workspace = Self::workspace_handle_for_config(config);
        if !self.session_exists(&workspace)? {
            self.create_workspace_session(config, &workspace)?;
        }
        self.sync_workspace_metadata(config, &workspace)?;
        Ok(workspace)
    }

    fn create_window(
        &self,
        workspace: &TmuxWorkspaceHandle,
        window_name: &str,
    ) -> Result<TmuxWindowHandle, Self::Error> {
        let args = vec![
            "new-window".to_string(),
            "-d".to_string(),
            "-P".to_string(),
            "-F".to_string(),
            "#{window_id}".to_string(),
            "-t".to_string(),
            workspace.session_name.as_str().to_string(),
            "-n".to_string(),
            window_name.to_string(),
        ];
        let output = self.run_workspace_command(workspace, &args)?;
        let window_id = parse_tmux_id(&output.stdout, '@', "window id")?;
        Ok(TmuxWindowHandle {
            workspace_id: workspace.workspace_id.clone(),
            window_id: TmuxWindowId::new(window_id),
        })
    }

    fn split_pane_right(
        &self,
        workspace: &TmuxWorkspaceHandle,
        window: &TmuxWindowHandle,
        width_percent: u8,
    ) -> Result<TmuxPaneId, Self::Error> {
        validate_percent(width_percent, "right split width")?;
        let args = vec![
            "split-window".to_string(),
            "-d".to_string(),
            "-P".to_string(),
            "-F".to_string(),
            "#{pane_id}".to_string(),
            "-t".to_string(),
            window.window_id.as_str().to_string(),
            "-h".to_string(),
            "-l".to_string(),
            format!("{width_percent}%"),
        ];
        let output = self.run_workspace_command(workspace, &args)?;
        Ok(TmuxPaneId::new(parse_tmux_id(
            &output.stdout,
            '%',
            "pane id",
        )?))
    }

    fn split_pane_bottom(
        &self,
        workspace: &TmuxWorkspaceHandle,
        window: &TmuxWindowHandle,
        height_percent: u8,
    ) -> Result<TmuxPaneId, Self::Error> {
        validate_percent(height_percent, "bottom split height")?;
        let args = vec![
            "split-window".to_string(),
            "-d".to_string(),
            "-P".to_string(),
            "-F".to_string(),
            "#{pane_id}".to_string(),
            "-t".to_string(),
            window.window_id.as_str().to_string(),
            "-v".to_string(),
            "-l".to_string(),
            format!("{height_percent}%"),
        ];
        let output = self.run_workspace_command(workspace, &args)?;
        Ok(TmuxPaneId::new(parse_tmux_id(
            &output.stdout,
            '%',
            "pane id",
        )?))
    }

    fn select_window(
        &self,
        workspace: &TmuxWorkspaceHandle,
        window: &TmuxWindowHandle,
    ) -> Result<(), Self::Error> {
        let args = vec![
            "select-window".to_string(),
            "-t".to_string(),
            window.window_id.as_str().to_string(),
        ];
        self.run_workspace_command(workspace, &args)?;
        Ok(())
    }

    fn select_pane(
        &self,
        workspace: &TmuxWorkspaceHandle,
        pane: &TmuxPaneId,
    ) -> Result<(), Self::Error> {
        let args = vec![
            "select-pane".to_string(),
            "-t".to_string(),
            pane.as_str().to_string(),
        ];
        self.run_workspace_command(workspace, &args)?;
        Ok(())
    }

    fn toggle_zoom(
        &self,
        workspace: &TmuxWorkspaceHandle,
        pane: &TmuxPaneId,
    ) -> Result<(), Self::Error> {
        let args = vec![
            "resize-pane".to_string(),
            "-t".to_string(),
            pane.as_str().to_string(),
            "-Z".to_string(),
        ];
        self.run_workspace_command(workspace, &args)?;
        Ok(())
    }

    fn enter_copy_mode(
        &self,
        workspace: &TmuxWorkspaceHandle,
        pane: &TmuxPaneId,
    ) -> Result<(), Self::Error> {
        let args = vec![
            "copy-mode".to_string(),
            "-t".to_string(),
            pane.as_str().to_string(),
        ];
        self.run_workspace_command(workspace, &args)?;
        Ok(())
    }
}

impl TmuxSessionGateway for EmbeddedTmuxBackend {
    fn list_sessions(&self) -> Result<Vec<ManagedSessionRecord>, Self::Error> {
        let mut sessions = Vec::new();
        for socket_name in self.discover_waitagent_sockets()? {
            sessions.extend(self.list_sessions_on_socket(&socket_name)?);
        }
        sessions.sort_by(|left, right| {
            left.address
                .server_id()
                .cmp(right.address.server_id())
                .then_with(|| left.address.session_id().cmp(right.address.session_id()))
        });
        Ok(sessions)
    }

    fn find_session(&self, target: &str) -> Result<Option<ManagedSessionRecord>, Self::Error> {
        self.find_managed_session(target)
    }

    fn attach_workspace(&self, workspace: &TmuxWorkspaceHandle) -> Result<(), Self::Error> {
        self.attach_to_socket_session(&workspace.socket_name, workspace.session_name.as_str())
    }

    fn attach_session(&self, address: &ManagedSessionAddress) -> Result<(), Self::Error> {
        self.attach_to_socket_session(
            &TmuxSocketName::new(address.server_id()),
            address.session_id(),
        )
    }

    fn detach_workspace_clients(&self, workspace: &TmuxWorkspaceHandle) -> Result<(), Self::Error> {
        self.detach_session_on_socket(&workspace.socket_name, workspace.session_name.as_str())
    }

    fn detach_session_clients(&self, address: &ManagedSessionAddress) -> Result<(), Self::Error> {
        self.detach_session_on_socket(
            &TmuxSocketName::new(address.server_id()),
            address.session_id(),
        )
    }

    fn detach_current_client(&self) -> Result<(), Self::Error> {
        self.command_runner()
            .run_from_current_client(&["detach-client".to_string()])
    }
}

impl TmuxChromeGateway for EmbeddedTmuxBackend {
    fn pane_dimensions_on_socket(
        &self,
        socket_name: &str,
        pane_target: &str,
    ) -> Result<(usize, usize), Self::Error> {
        EmbeddedTmuxBackend::pane_dimensions_on_socket(self, socket_name, pane_target)
    }

    fn window_zoomed_on_socket(
        &self,
        socket_name: &str,
        pane_target: &str,
    ) -> Result<bool, Self::Error> {
        EmbeddedTmuxBackend::window_zoomed_on_socket(self, socket_name, pane_target)
    }

    fn show_session_option(
        &self,
        workspace: &TmuxWorkspaceHandle,
        option_name: &str,
    ) -> Result<Option<String>, Self::Error> {
        EmbeddedTmuxBackend::show_session_option(self, workspace, option_name)
    }
}

#[cfg(test)]
mod tests {
    use super::EmbeddedTmuxBackend;
    use crate::domain::workspace::{
        WorkspaceInstanceConfig, WorkspaceInstanceId, WorkspaceSessionRole,
    };
    use crate::infra::tmux_error::tmux_socket_dir;
    use crate::infra::tmux_glue::TmuxGlueBuildStatus;
    use crate::infra::tmux_types::{
        TmuxGateway, TmuxLayoutGateway, TmuxProgram, TmuxSessionGateway, TmuxSessionName,
        TmuxSocketName, TmuxSplitSize, TmuxWindowHandle, TmuxWindowId, TmuxWorkspaceHandle,
    };
    use std::path::Path;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn workspace_config() -> WorkspaceInstanceConfig {
        WorkspaceInstanceConfig {
            workspace_dir: Path::new("/tmp").to_path_buf(),
            workspace_key: "wk-1".to_string(),
            socket_name: "sock-1".to_string(),
            session_name: "sess-1".to_string(),
            session_role: WorkspaceSessionRole::WorkspaceChrome,
            initial_rows: None,
            initial_cols: None,
        }
    }

    fn workspace_handle() -> TmuxWorkspaceHandle {
        TmuxWorkspaceHandle {
            workspace_id: WorkspaceInstanceId::new("wk-1"),
            socket_name: TmuxSocketName::new("sock-1"),
            session_name: TmuxSessionName::new("sess-1"),
        }
    }

    #[test]
    fn embedded_backend_returns_workspace_handle_from_build_env() {
        let backend = EmbeddedTmuxBackend::from_build_env()
            .expect("vendored tmux backend should discover build env");

        let handle = backend
            .ensure_workspace(&workspace_config())
            .expect("workspace handle should build");
        kill_server(&backend, &handle);

        assert_eq!(handle.workspace_id.as_str(), "wk-1");
        assert_eq!(handle.socket_name.as_str(), "sock-1");
        assert_eq!(handle.session_name.as_str(), "sess-1");
        assert_eq!(backend.build_status(), &TmuxGlueBuildStatus::Executed);
        assert!(backend
            .artifacts()
            .static_lib_path
            .to_string_lossy()
            .ends_with("/lib/libtmux-glue.a"));
    }

    #[test]
    fn embedded_backend_reuses_existing_workspace_session() {
        let backend = EmbeddedTmuxBackend::from_build_env()
            .expect("vendored tmux backend should discover build env");
        let config = unique_workspace_config("workspace");
        let workspace = backend
            .ensure_workspace(&config)
            .expect("workspace bootstrap should succeed");
        let workspace_again = backend
            .ensure_workspace(&config)
            .expect("workspace bootstrap should be idempotent");

        let sessions = backend
            .list_sessions_on_socket(&workspace.socket_name)
            .expect("session list should succeed");
        kill_server(&backend, &workspace);

        let matching = sessions
            .into_iter()
            .filter(|record| record.address.session_id() == workspace.session_name.as_str())
            .count();

        assert_eq!(workspace, workspace_again);
        assert_eq!(matching, 1);
    }

    #[test]
    fn embedded_backend_executes_real_window_and_pane_commands() {
        let backend = EmbeddedTmuxBackend::from_build_env()
            .expect("vendored tmux backend should discover build env");
        let workspace = backend
            .ensure_workspace(&unique_workspace_config("layout"))
            .expect("workspace bootstrap should succeed");

        let created_window = backend
            .create_window(&workspace, "codex")
            .expect("window handle should build");
        let right = backend
            .split_pane_right(&workspace, &created_window, 24)
            .expect("right pane should build");
        let bottom = backend
            .split_pane_bottom(&workspace, &created_window, 18)
            .expect("bottom pane should build");
        backend
            .select_window(&workspace, &created_window)
            .expect("window selection should succeed");
        backend
            .select_pane(&workspace, &right)
            .expect("pane selection should succeed");
        backend
            .toggle_zoom(&workspace, &right)
            .expect("pane zoom should succeed");
        backend
            .enter_copy_mode(&workspace, &right)
            .expect("copy mode should succeed");

        let panes = backend
            .list_panes(&workspace, &created_window)
            .expect("pane listing should succeed");
        kill_server(&backend, &workspace);

        let active_pane = panes
            .iter()
            .find(|pane| pane.pane_id == right)
            .expect("split pane should exist");

        assert!(created_window.window_id.as_str().starts_with('@'));
        assert!(right.as_str().starts_with('%'));
        assert!(bottom.as_str().starts_with('%'));
        assert_eq!(active_pane.pane_id, right);
    }

    #[test]
    fn embedded_backend_sets_new_workspace_history_limit_before_session_creation() {
        let backend = EmbeddedTmuxBackend::from_build_env()
            .expect("vendored tmux backend should discover build env");
        let workspace = backend
            .ensure_workspace(&unique_workspace_config("history-limit"))
            .expect("workspace bootstrap should succeed");

        let output = backend
            .run_on_socket(
                &workspace.socket_name,
                &[
                    "show-options".to_string(),
                    "-g".to_string(),
                    "history-limit".to_string(),
                ],
            )
            .expect("history-limit should be visible");
        kill_server(&backend, &workspace);

        assert!(output.stdout.contains("history-limit 100000"));
    }

    #[test]
    fn embedded_backend_reports_current_window_and_runs_pane_programs() {
        let backend = EmbeddedTmuxBackend::from_build_env()
            .expect("vendored tmux backend should discover build env");
        let workspace = backend
            .ensure_workspace(&unique_workspace_config("pane-prog"))
            .expect("workspace bootstrap should succeed");
        let window = backend
            .current_window(&workspace)
            .expect("current window should resolve");
        let main = backend
            .current_pane(&workspace)
            .expect("current pane should resolve");
        let program =
            TmuxProgram::new("/bin/sh").with_args(vec!["-c".to_string(), "sleep 30".to_string()]);

        let sidebar = backend
            .split_pane_right_with_program(&workspace, &main, TmuxSplitSize::Cells(24), &program)
            .expect("program-backed sidebar pane should spawn");
        backend
            .set_pane_title(&workspace, &sidebar, "waitagent-sidebar")
            .expect("pane title should be set");
        backend
            .set_pane_width(&workspace, &sidebar, 24)
            .expect("sidebar width should be set");
        let footer = backend
            .split_pane_bottom_with_program(
                &workspace,
                &main,
                TmuxSplitSize::Cells(2),
                true,
                &program,
            )
            .expect("program-backed footer pane should spawn");
        backend
            .set_pane_title(&workspace, &footer, "waitagent-footer")
            .expect("footer pane title should be set");
        backend
            .set_pane_height(&workspace, &footer, 2)
            .expect("footer height should be set");
        let panes = backend
            .list_panes(&workspace, &window)
            .expect("pane listing should succeed");
        kill_server(&backend, &workspace);

        assert!(panes.iter().any(|pane| pane.title == "waitagent-sidebar"));
        assert!(panes.iter().any(|pane| pane.title == "waitagent-footer"));
    }

    #[test]
    fn split_percentages_must_be_nonzero() {
        let backend = EmbeddedTmuxBackend::from_build_env()
            .expect("vendored tmux backend should discover build env");
        let workspace = workspace_handle();
        let window = TmuxWindowHandle {
            workspace_id: WorkspaceInstanceId::new("wk-1"),
            window_id: TmuxWindowId::new("@3"),
        };

        let error = backend
            .split_pane_right(&workspace, &window, 0)
            .expect_err("zero-width split should fail");

        assert!(error.to_string().contains("right split width"));
    }

    #[test]
    fn embedded_backend_lists_waitagent_sessions_with_workspace_metadata() {
        let backend = EmbeddedTmuxBackend::from_build_env()
            .expect("vendored tmux backend should discover build env");
        let config = unique_workspace_config("listing");
        let workspace = backend
            .ensure_workspace(&config)
            .expect("workspace bootstrap should succeed");

        let sessions = backend
            .list_sessions()
            .expect("managed session listing should succeed");
        kill_server(&backend, &workspace);

        let record = sessions
            .into_iter()
            .find(|session| session.address.session_id() == workspace.session_name.as_str())
            .expect("workspace session should be listed");

        assert_eq!(record.address.server_id(), workspace.socket_name.as_str());
        assert_eq!(
            record.workspace_dir.as_deref(),
            Some(config.workspace_dir.as_path())
        );
        assert_eq!(
            record.workspace_key.as_deref(),
            Some(config.workspace_key.as_str())
        );
    }

    #[test]
    fn tmux_socket_dir_matches_tmux_uid_convention() {
        let socket_dir = tmux_socket_dir();
        assert!(socket_dir.to_string_lossy().contains("/tmux-"));
    }

    #[test]
    fn chrome_refresh_signal_wakes_multiple_workspace_waiters() {
        let backend = EmbeddedTmuxBackend::from_build_env()
            .expect("vendored tmux backend should discover build env");
        let workspace = backend
            .ensure_workspace(&unique_workspace_config("chrome-refresh"))
            .expect("workspace bootstrap should succeed");
        let (done_tx, done_rx) = mpsc::channel();

        for _ in 0..2 {
            let backend = backend.clone();
            let socket_name = workspace.socket_name.as_str().to_string();
            let session_name = workspace.session_name.as_str().to_string();
            let done_tx = done_tx.clone();
            thread::spawn(move || {
                backend
                    .wait_for_chrome_refresh_on_socket(&socket_name, &session_name)
                    .expect("wait-for should unblock cleanly");
                done_tx
                    .send(())
                    .expect("waiter completion should be reported");
            });
        }

        thread::sleep(Duration::from_millis(100));
        backend
            .signal_chrome_refresh_on_socket(
                workspace.socket_name.as_str(),
                workspace.session_name.as_str(),
            )
            .expect("chrome refresh signal should succeed");

        done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("first waiter should wake");
        done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("second waiter should wake");
        kill_server(&backend, &workspace);
    }

    fn unique_workspace_config(prefix: &str) -> WorkspaceInstanceConfig {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after unix epoch")
            .as_nanos();
        let workspace_dir = std::env::temp_dir().join(format!("waitagent-{prefix}-{nonce:x}"));
        std::fs::create_dir_all(&workspace_dir)
            .expect("temporary workspace directory should be created");
        WorkspaceInstanceConfig {
            workspace_dir,
            workspace_key: format!("{prefix}-{nonce:x}"),
            socket_name: format!("wa-test-{nonce:x}"),
            session_name: format!("waitagent-test-{prefix}-{nonce:x}"),
            session_role: WorkspaceSessionRole::WorkspaceChrome,
            initial_rows: None,
            initial_cols: None,
        }
    }

    fn kill_server(backend: &EmbeddedTmuxBackend, workspace: &TmuxWorkspaceHandle) {
        let _ = backend.run_workspace_command(workspace, &["kill-server".to_string()]);
    }
}
