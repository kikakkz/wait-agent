use crate::cli::RemoteNetworkConfig;
use crate::domain::session_catalog::ManagedSessionRecord;
use crate::infra::tmux::RemoteTargetPublicationBinding;
use crate::lifecycle::LifecycleError;
use crate::runtime::ratatui_node::state_event::StateEvent;
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
        Ok(guard
            .values()
            .any(|session| session.address.session_id() == session_name))
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
        // The target carries the remote-peer transport prefix used by the sync
        // protocol (e.g. `remote-peer:10.1.29.9#7575:1`).  The local catalog
        // keys authority-host sessions under `local#<port>:<id>`, so map the
        // server's own node id back to the local form before removing.
        let qualified_target = target
            .strip_prefix("remote-peer:")
            .or_else(|| target.strip_prefix("local:"))
            .unwrap_or(target);
        if qualified_target.is_empty() {
            return Ok(());
        }
        let local_target = {
            let (authority_id, session_id) = qualified_target
                .rsplit_once(':')
                .unwrap_or((qualified_target, ""));
            if authority_id == self.shared.network.advertised_node_id() {
                format!("{}:{session_id}", self.shared.local_authority_id())
            } else {
                qualified_target.to_string()
            }
        };
        let _ = self.shared.state_sender().send(StateEvent::SessionClosed {
            target_id: local_target,
        });
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

    fn signal_remote_node_offline(&self, node_id: &str) -> Result<(), LifecycleError> {
        let _ = self
            .shared
            .state_sender()
            .send(StateEvent::RemoteNodeOffline {
                node_id: node_id.to_string(),
            });
        Ok(())
    }

    fn refresh_workspace_socket(
        &self,
        socket_name: &str,
        _executable: &Path,
    ) -> Result<(), LifecycleError> {
        if !self.socket_is_live(socket_name) {
            return Ok(());
        }
        // Ask the single writer loop to broadcast a snapshot.  The target id is
        // ignored for this refresh event.
        let _ = self
            .shared
            .state_sender()
            .send(StateEvent::LocalSessionOutput {
                target_id: String::new(),
            });
        Ok(())
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
