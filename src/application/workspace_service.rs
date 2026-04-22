use crate::domain::workspace::WorkspaceInstanceConfig;
use crate::infra::tmux::{TmuxGateway, TmuxWorkspaceHandle};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootstrappedWorkspace {
    pub workspace_dir: PathBuf,
    pub instance_config: WorkspaceInstanceConfig,
    pub workspace_handle: TmuxWorkspaceHandle,
}

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

    pub fn ensure_workspace_for_dir(
        &self,
        workspace_dir: &Path,
    ) -> Result<BootstrappedWorkspace, G::Error> {
        let instance_config = WorkspaceInstanceConfig::for_workspace_dir(workspace_dir);
        let workspace_handle = self.ensure_workspace(&instance_config)?;

        Ok(BootstrappedWorkspace {
            workspace_dir: workspace_dir.to_path_buf(),
            instance_config,
            workspace_handle,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{BootstrappedWorkspace, WorkspaceService};
    use crate::domain::workspace::WorkspaceInstanceId;
    use crate::infra::tmux::{TmuxGateway, TmuxSessionName, TmuxSocketName, TmuxWorkspaceHandle};
    use std::path::Path;

    #[derive(Debug, Clone)]
    struct FakeTmuxGateway;

    impl TmuxGateway for FakeTmuxGateway {
        type Error = &'static str;

        fn ensure_workspace(
            &self,
            config: &crate::domain::workspace::WorkspaceInstanceConfig,
        ) -> Result<TmuxWorkspaceHandle, Self::Error> {
            Ok(TmuxWorkspaceHandle {
                workspace_id: WorkspaceInstanceId::new(config.workspace_key.clone()),
                socket_name: TmuxSocketName::new(config.socket_name.clone()),
                session_name: TmuxSessionName::new(config.session_name.clone()),
            })
        }

        fn create_window(
            &self,
            _workspace: &TmuxWorkspaceHandle,
            _window_name: &str,
        ) -> Result<crate::infra::tmux::TmuxWindowHandle, Self::Error> {
            unreachable!("not used in this test")
        }

        fn split_pane_right(
            &self,
            _workspace: &TmuxWorkspaceHandle,
            _window: &crate::infra::tmux::TmuxWindowHandle,
            _width_percent: u8,
        ) -> Result<crate::infra::tmux::TmuxPaneId, Self::Error> {
            unreachable!("not used in this test")
        }

        fn split_pane_bottom(
            &self,
            _workspace: &TmuxWorkspaceHandle,
            _window: &crate::infra::tmux::TmuxWindowHandle,
            _height_percent: u8,
        ) -> Result<crate::infra::tmux::TmuxPaneId, Self::Error> {
            unreachable!("not used in this test")
        }

        fn select_window(
            &self,
            _workspace: &TmuxWorkspaceHandle,
            _window: &crate::infra::tmux::TmuxWindowHandle,
        ) -> Result<(), Self::Error> {
            unreachable!("not used in this test")
        }

        fn select_pane(
            &self,
            _workspace: &TmuxWorkspaceHandle,
            _pane: &crate::infra::tmux::TmuxPaneId,
        ) -> Result<(), Self::Error> {
            unreachable!("not used in this test")
        }

        fn toggle_zoom(
            &self,
            _workspace: &TmuxWorkspaceHandle,
            _pane: &crate::infra::tmux::TmuxPaneId,
        ) -> Result<(), Self::Error> {
            unreachable!("not used in this test")
        }

        fn enter_copy_mode(
            &self,
            _workspace: &TmuxWorkspaceHandle,
            _pane: &crate::infra::tmux::TmuxPaneId,
        ) -> Result<(), Self::Error> {
            unreachable!("not used in this test")
        }
    }

    #[test]
    fn ensure_workspace_for_dir_derives_tmux_workspace_identity() {
        let service = WorkspaceService::new(FakeTmuxGateway);
        let workspace = service
            .ensure_workspace_for_dir(Path::new("/tmp/waitagent/ws"))
            .expect("workspace bootstrap should succeed");

        let BootstrappedWorkspace {
            workspace_dir,
            instance_config,
            workspace_handle,
        } = workspace;

        assert_eq!(workspace_dir, Path::new("/tmp/waitagent/ws"));
        assert_eq!(
            workspace_handle.workspace_id.as_str(),
            instance_config.workspace_key
        );
        assert_eq!(
            workspace_handle.socket_name.as_str(),
            instance_config.socket_name
        );
        assert_eq!(
            workspace_handle.session_name.as_str(),
            instance_config.session_name
        );
    }
}
