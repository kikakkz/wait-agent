use crate::cli::RemoteNetworkConfig;
use crate::domain::session_catalog::ManagedSessionRecord;
use crate::infra::tmux::RemoteTargetPublicationBinding;
use crate::lifecycle::LifecycleError;
use crate::runtime::ratatui_node::SharedState;
use crate::runtime::remote_publication::remote_target_publication_backend::RemoteTargetPublicationBackend;
use std::path::Path;
use std::sync::Arc;

/// Ratatui-backed implementation of `RemoteTargetPublicationBackend`.
///
/// In the single-process ratatui server model, sessions live in an in-memory
/// `SharedState` catalog and the TUI receives snapshots broadcast from the
/// server. Hooks, sidecars, and tmux socket operations are no-ops here.
#[derive(Clone)]
pub struct RatatuiRemoteTargetPublicationBackend {
    shared: Arc<SharedState>,
    network: RemoteNetworkConfig,
}

impl RatatuiRemoteTargetPublicationBackend {
    #[allow(dead_code)]
    pub fn new(shared: Arc<SharedState>, network: RemoteNetworkConfig) -> Self {
        Self { shared, network }
    }

    fn workspace_socket_name(&self) -> String {
        format!("ratatui-{}", self.network.port)
    }
}

impl RemoteTargetPublicationBackend for RatatuiRemoteTargetPublicationBackend {
    fn live_workspace_socket_names(
        &self,
        _network: &RemoteNetworkConfig,
    ) -> Result<Vec<String>, LifecycleError> {
        Ok(vec![self.workspace_socket_name()])
    }

    fn socket_is_live(&self, socket_name: &str) -> bool {
        socket_name == self.workspace_socket_name()
    }

    fn list_sessions_on_socket(
        &self,
        socket_name: &str,
    ) -> Result<Vec<ManagedSessionRecord>, LifecycleError> {
        if !self.socket_is_live(socket_name) {
            return Ok(Vec::new());
        }
        let guard = self.shared.sessions.lock().unwrap();
        Ok(guard.values().cloned().collect())
    }

    fn find_publication_binding(
        &self,
        _socket_name: &str,
        _target_session_name: &str,
    ) -> Result<Option<RemoteTargetPublicationBinding>, LifecycleError> {
        // Bindings are not yet wired into the ratatui SharedState catalog.
        Ok(None)
    }

    fn list_publication_bindings(
        &self,
        _socket_name: &str,
    ) -> Result<Vec<RemoteTargetPublicationBinding>, LifecycleError> {
        Ok(Vec::new())
    }

    fn bind_publication(
        &self,
        _socket_name: &str,
        _target_session_name: &str,
        _authority_id: &str,
        _transport_session_id: &str,
        _selector: Option<&str>,
    ) -> Result<(), LifecycleError> {
        // No-op until ratatui publication wiring is added.
        Ok(())
    }

    fn unbind_publication(
        &self,
        _socket_name: &str,
        _target_session_name: &str,
    ) -> Result<(), LifecycleError> {
        // No-op until ratatui publication wiring is added.
        Ok(())
    }

    fn live_content_pane_for_session(
        &self,
        socket_name: &str,
        session_name: &str,
    ) -> Result<bool, LifecycleError> {
        if !self.socket_is_live(socket_name) {
            return Ok(false);
        }
        let guard = self.shared.sessions.lock().unwrap();
        Ok(guard.contains_key(session_name))
    }

    fn ensure_publication_hooks(
        &self,
        _socket_name: &str,
        _network: &RemoteNetworkConfig,
    ) -> Result<(), LifecycleError> {
        // Tmux hooks do not exist in the ratatui single-process model.
        Ok(())
    }

    fn signal_remote_target_exited(
        &self,
        socket_name: &str,
        _session_name: &str,
        target: &str,
        _executable: &Path,
    ) -> Result<(), LifecycleError> {
        if !self.socket_is_live(socket_name) {
            return Ok(());
        }
        let Some((authority_id, transport_session_id)) = target.split_once(':') else {
            return Ok(());
        };
        let exited_session_id = {
            let guard = self.shared.sessions.lock().unwrap();
            guard
                .iter()
                .find(|(_, session)| {
                    session.address.authority_id() == authority_id
                        && session.address.session_id() == transport_session_id
                })
                .map(|(session_id, _)| session_id.clone())
        };
        if let Some(session_id) = exited_session_id {
            self.shared.handle_session_exit(&session_id);
        }
        Ok(())
    }

    fn signal_remote_target_exited_to_workspace(
        &self,
        socket_name: &str,
        target: &str,
        executable: &Path,
    ) -> Result<usize, LifecycleError> {
        self.signal_remote_target_exited(socket_name, "", target, executable)?;
        Ok(1)
    }

    fn refresh_workspace_socket(
        &self,
        socket_name: &str,
        _executable: &Path,
    ) -> Result<(), LifecycleError> {
        if !self.socket_is_live(socket_name) {
            return Ok(());
        }
        self.shared.broadcast_snapshot()
    }

    fn ensure_publication_server_running(
        &self,
        _socket_name: &str,
        _network: &RemoteNetworkConfig,
        _executable: &Path,
    ) -> Result<(), LifecycleError> {
        // Single-process server: no separate publication server sidecar.
        Ok(())
    }

    fn ensure_publication_agent_running(
        &self,
        _socket_name: &str,
        _network: &RemoteNetworkConfig,
        _executable: &Path,
    ) -> Result<(), LifecycleError> {
        Ok(())
    }

    fn ensure_publication_sender_running(
        &self,
        _socket_name: &str,
        _network: &RemoteNetworkConfig,
        _executable: &Path,
    ) -> Result<(), LifecycleError> {
        Ok(())
    }

    fn ensure_publication_owner_running(
        &self,
        _socket_name: &str,
        _target_session_name: &str,
        _network: &RemoteNetworkConfig,
        _executable: &Path,
    ) -> Result<(), LifecycleError> {
        Ok(())
    }
}
