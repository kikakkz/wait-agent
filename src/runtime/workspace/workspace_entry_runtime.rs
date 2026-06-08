use crate::application::workspace_service::BootstrappedWorkspace;
use crate::infra::tmux::{EmbeddedTmuxBackend, TmuxError};
use crate::lifecycle::LifecycleError;
use crate::runtime::network_state_runtime::persist_workspace_network_config;
use crate::runtime::workspace_layout_runtime::WorkspaceLayoutRuntime;
use crate::runtime::workspace_runtime::WorkspaceRuntime;
use crate::terminal::TerminalRuntime;
use std::io;
use std::path::Path;

pub struct WorkspaceEntryRuntime {
    workspace_runtime: WorkspaceRuntime<EmbeddedTmuxBackend>,
    layout_runtime: WorkspaceLayoutRuntime,
    network: crate::cli::RemoteNetworkConfig,
}

impl WorkspaceEntryRuntime {
    #[cfg(test)]
    pub fn new(
        workspace_runtime: WorkspaceRuntime<EmbeddedTmuxBackend>,
        layout_runtime: WorkspaceLayoutRuntime,
    ) -> Self {
        Self::new_with_network(
            workspace_runtime,
            layout_runtime,
            crate::cli::RemoteNetworkConfig::default(),
        )
    }

    pub fn new_with_network(
        workspace_runtime: WorkspaceRuntime<EmbeddedTmuxBackend>,
        layout_runtime: WorkspaceLayoutRuntime,
        network: crate::cli::RemoteNetworkConfig,
    ) -> Self {
        Self {
            workspace_runtime,
            layout_runtime,
            network,
        }
    }

    pub fn bootstrap_workspace(
        &self,
        workspace_dir: &Path,
    ) -> Result<BootstrappedWorkspace, LifecycleError> {
        let terminal_size = TerminalRuntime::stdio().current_size_or_default();
        let (rows, cols) = if terminal_size.rows > 1 && terminal_size.cols > 1 {
            (Some(terminal_size.rows), Some(terminal_size.cols))
        } else {
            (None, None)
        };
        let workspace = self
            .workspace_runtime
            .ensure_workspace_for_dir_with_size(workspace_dir, rows, cols)
            .map_err(tmux_bootstrap_error)?;
        persist_workspace_network_config(
            self.workspace_runtime.backend(),
            &workspace.workspace_handle,
            &self.network,
        )
        .map_err(tmux_bootstrap_error)?;
        self.layout_runtime
            .ensure_layout(&workspace.workspace_handle, &workspace.workspace_dir)?;
        Ok(workspace)
    }
}

fn tmux_bootstrap_error(error: TmuxError) -> LifecycleError {
    LifecycleError::Io(
        "failed to bootstrap tmux-backed workspace instance".to_string(),
        io::Error::new(io::ErrorKind::Other, error.to_string()),
    )
}
