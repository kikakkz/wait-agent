use crate::application::workspace_service::BootstrappedWorkspace;
use crate::infra::per_server_geometry_store::{default_store_path, PerServerGeometryStore};
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
        let terminal = TerminalRuntime::stdio();
        let terminal_size = terminal.current_size_or_default();
        let (rows, cols) =
            if terminal_size.rows > 1 && terminal_size.cols > 1 && terminal.output_is_tty() {
                (Some(terminal_size.rows), Some(terminal_size.cols))
            } else {
                // Headless creation (managed node daemon): prefer the last
                // negotiated geometry stored for the connecting server over the
                // tmux 80x24 detached default.  The stored geometry is the
                // main pane size; add the standard chrome overhead (32-wide
                // sidebar plus border, 1-row footer plus border) so the pane
                // starts at the negotiated size.
                self.network
                    .connect
                    .as_deref()
                    .and_then(|server| {
                        let store = PerServerGeometryStore::load(&default_store_path());
                        store
                            .lookup(server)
                            .map(|geometry| (geometry.rows + 2, geometry.cols + 33))
                    })
                    .map_or((None, None), |(rows, cols)| {
                        (Some(rows as u16), Some(cols as u16))
                    })
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
