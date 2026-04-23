use crate::application::control_service::ControlService;
use crate::application::layout_service::{LayoutFocusBehavior, LayoutService};
use crate::cli::LayoutReconcileCommand;
use crate::domain::workspace::WorkspaceInstanceId;
use crate::infra::tmux::{
    EmbeddedTmuxBackend, TmuxError, TmuxProgram, TmuxSessionName, TmuxSocketName,
    TmuxWorkspaceHandle,
};
use crate::lifecycle::LifecycleError;
use std::io;
use std::path::{Path, PathBuf};

pub struct WorkspaceLayoutRuntime {
    control_service: ControlService<EmbeddedTmuxBackend>,
    layout_service: LayoutService<EmbeddedTmuxBackend>,
    current_executable: std::path::PathBuf,
}

impl WorkspaceLayoutRuntime {
    pub fn from_build_env() -> Result<Self, LifecycleError> {
        let backend = EmbeddedTmuxBackend::from_build_env().map_err(tmux_layout_error)?;
        let current_executable = std::env::current_exe().map_err(|error| {
            LifecycleError::Io(
                "failed to locate current waitagent executable".to_string(),
                error,
            )
        })?;

        Ok(Self {
            control_service: ControlService::new(backend.clone()),
            layout_service: LayoutService::new(backend),
            current_executable,
        })
    }

    pub fn ensure_layout(
        &self,
        workspace: &TmuxWorkspaceHandle,
        workspace_dir: &Path,
    ) -> Result<(), LifecycleError> {
        self.reconcile_layout(workspace, workspace_dir, LayoutFocusBehavior::ReturnToMain)
    }

    pub fn run_reconcile(&self, command: LayoutReconcileCommand) -> Result<(), LifecycleError> {
        let workspace_dir = PathBuf::from(&command.workspace_dir);
        let workspace = TmuxWorkspaceHandle {
            workspace_id: WorkspaceInstanceId::new(command.session_name.clone()),
            socket_name: TmuxSocketName::new(command.socket_name),
            session_name: TmuxSessionName::new(command.session_name),
        };
        self.reconcile_layout(
            &workspace,
            &workspace_dir,
            LayoutFocusBehavior::PreserveCurrent,
        )
    }

    fn reconcile_layout(
        &self,
        workspace: &TmuxWorkspaceHandle,
        workspace_dir: &Path,
        focus_behavior: LayoutFocusBehavior,
    ) -> Result<(), LifecycleError> {
        let sidebar_program = self.sidebar_program(workspace, workspace_dir);
        let footer_program = self.footer_program(workspace, workspace_dir);
        let reconcile_command = self.layout_reconcile_hook_command(workspace, workspace_dir);
        let layout = self
            .layout_service
            .ensure_workspace_layout(workspace, &sidebar_program, &footer_program, focus_behavior)
            .map_err(tmux_layout_error)?;
        self.control_service
            .ensure_native_controls(workspace, &layout)
            .map_err(tmux_layout_error)?;
        self.layout_service
            .ensure_layout_hooks(workspace, &reconcile_command)
            .map(|_| ())
            .map_err(tmux_layout_error)
    }

    fn sidebar_program(
        &self,
        workspace: &TmuxWorkspaceHandle,
        workspace_dir: &Path,
    ) -> TmuxProgram {
        TmuxProgram::new(self.current_executable.display().to_string())
            .with_args(vec![
                "__ui-sidebar".to_string(),
                "--socket-name".to_string(),
                workspace.socket_name.as_str().to_string(),
                "--session-name".to_string(),
                workspace.session_name.as_str().to_string(),
            ])
            .with_start_directory(workspace_dir)
    }

    fn footer_program(&self, workspace: &TmuxWorkspaceHandle, workspace_dir: &Path) -> TmuxProgram {
        TmuxProgram::new(self.current_executable.display().to_string())
            .with_args(vec![
                "__ui-footer".to_string(),
                "--socket-name".to_string(),
                workspace.socket_name.as_str().to_string(),
                "--session-name".to_string(),
                workspace.session_name.as_str().to_string(),
            ])
            .with_start_directory(workspace_dir)
    }

    fn layout_reconcile_hook_command(
        &self,
        workspace: &TmuxWorkspaceHandle,
        workspace_dir: &Path,
    ) -> String {
        let shell_command = [
            shell_escape(self.current_executable.to_string_lossy().as_ref()),
            shell_escape("__layout-reconcile"),
            shell_escape("--socket-name"),
            shell_escape(workspace.socket_name.as_str()),
            shell_escape("--session-name"),
            shell_escape(workspace.session_name.as_str()),
            shell_escape("--workspace-dir"),
            shell_escape(&workspace_dir.display().to_string()),
        ]
        .join(" ");
        format!(
            "run-shell -b {}",
            shell_escape(&format!("{shell_command} >/dev/null 2>&1"))
        )
    }
}

fn shell_escape(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn tmux_layout_error(error: TmuxError) -> LifecycleError {
    LifecycleError::Io(
        "failed to ensure tmux-owned waitagent layout".to_string(),
        io::Error::new(io::ErrorKind::Other, error.to_string()),
    )
}
