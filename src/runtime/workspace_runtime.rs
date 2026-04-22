use crate::application::workspace_service::WorkspaceService;
use crate::domain::workspace::WorkspaceInstanceConfig;
use crate::infra::tmux::{TmuxGateway, TmuxWorkspaceHandle};

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
}
