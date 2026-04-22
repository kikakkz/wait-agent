use crate::domain::workspace::WorkspaceInstanceConfig;
use crate::infra::tmux::{TmuxGateway, TmuxWorkspaceHandle};

pub struct WorkspaceService<G> {
    tmux: G,
}

impl<G> WorkspaceService<G>
where
    G: TmuxGateway,
{
    pub fn new(tmux: G) -> Self {
        Self { tmux }
    }

    pub fn ensure_workspace(
        &self,
        config: &WorkspaceInstanceConfig,
    ) -> Result<TmuxWorkspaceHandle, G::Error> {
        self.tmux.ensure_workspace(config)
    }
}
