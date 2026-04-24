use crate::application::workspace_service::{BootstrappedWorkspace, WorkspaceService};
use crate::domain::workspace::WorkspaceInstanceConfig;
use crate::infra::tmux::{TmuxGateway, TmuxWorkspaceHandle};
use std::path::Path;

pub struct WorkspaceRuntime<G> {
    workspace_service: WorkspaceService<G>,
}

impl<G> WorkspaceRuntime<G>
where
    G: TmuxGateway,
{
    pub fn new(workspace_service: WorkspaceService<G>) -> Self {
        Self { workspace_service }
    }

    pub fn ensure_workspace(
        &self,
        config: &WorkspaceInstanceConfig,
    ) -> Result<TmuxWorkspaceHandle, G::Error> {
        self.workspace_service.ensure_workspace(config)
    }

    pub fn ensure_workspace_for_dir(
        &self,
        workspace_dir: &Path,
    ) -> Result<BootstrappedWorkspace, G::Error> {
        self.workspace_service
            .ensure_workspace_for_dir(workspace_dir)
    }

    pub fn ensure_workspace_for_dir_with_size(
        &self,
        workspace_dir: &Path,
        rows: Option<u16>,
        cols: Option<u16>,
    ) -> Result<BootstrappedWorkspace, G::Error> {
        self.workspace_service
            .ensure_workspace_for_dir_with_size(workspace_dir, rows, cols)
    }
}
