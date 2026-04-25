use crate::application::workspace_service::{BootstrappedWorkspace, WorkspaceService};
use crate::domain::workspace::WorkspaceInstanceConfig;
use crate::infra::tmux::{EmbeddedTmuxBackend, TmuxError};
use crate::runtime::workspace_runtime::WorkspaceRuntime;

pub struct TargetHostRuntime {
    workspace_runtime: WorkspaceRuntime<EmbeddedTmuxBackend>,
}

impl TargetHostRuntime {
    pub fn new(workspace_runtime: WorkspaceRuntime<EmbeddedTmuxBackend>) -> Self {
        Self { workspace_runtime }
    }

    pub fn from_backend(backend: EmbeddedTmuxBackend) -> Self {
        Self::new(WorkspaceRuntime::new(WorkspaceService::new(backend)))
    }

    pub fn ensure_target_host(
        &self,
        config: WorkspaceInstanceConfig,
    ) -> Result<BootstrappedWorkspace, TmuxError> {
        self.workspace_runtime.ensure_workspace_for_config(config)
    }
}
