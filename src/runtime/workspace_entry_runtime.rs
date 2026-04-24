use crate::application::workspace_service::{BootstrappedWorkspace, WorkspaceService};
use crate::infra::tmux::{EmbeddedTmuxBackend, TmuxError};
use crate::lifecycle::LifecycleError;
use crate::runtime::workspace_layout_runtime::WorkspaceLayoutRuntime;
use crate::runtime::workspace_runtime::WorkspaceRuntime;
use std::io;
use std::path::Path;

pub struct WorkspaceEntryRuntime {
    workspace_runtime: WorkspaceRuntime<EmbeddedTmuxBackend>,
    layout_runtime: WorkspaceLayoutRuntime,
}

impl WorkspaceEntryRuntime {
    pub fn new(
        workspace_runtime: WorkspaceRuntime<EmbeddedTmuxBackend>,
        layout_runtime: WorkspaceLayoutRuntime,
    ) -> Self {
        Self {
            workspace_runtime,
            layout_runtime,
        }
    }

    pub fn from_build_env() -> Result<Self, LifecycleError> {
        let backend = EmbeddedTmuxBackend::from_build_env().map_err(tmux_bootstrap_error)?;
        let workspace_service = WorkspaceService::new(backend);
        let layout_runtime = WorkspaceLayoutRuntime::from_build_env()?;
        Ok(Self::new(
            WorkspaceRuntime::new(workspace_service),
            layout_runtime,
        ))
    }

    pub fn bootstrap_workspace(
        &self,
        workspace_dir: &Path,
    ) -> Result<BootstrappedWorkspace, LifecycleError> {
        let workspace = self
            .workspace_runtime
            .ensure_workspace_for_dir(workspace_dir)
            .map_err(tmux_bootstrap_error)?;
        self.layout_runtime
            .ensure_layout(&workspace.workspace_handle, &workspace.workspace_dir)?;
        self.layout_runtime.run_chrome_refresh_all()?;
        Ok(workspace)
    }
}

fn tmux_bootstrap_error(error: TmuxError) -> LifecycleError {
    LifecycleError::Io(
        "failed to bootstrap tmux-backed workspace instance".to_string(),
        io::Error::new(io::ErrorKind::Other, error.to_string()),
    )
}
