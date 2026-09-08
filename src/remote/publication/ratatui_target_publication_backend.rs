use crate::cli::RemoteNetworkConfig;
use crate::domain::session_catalog::ManagedSessionRecord;
use crate::infra::error_log::ERROR_LOG;
use crate::lifecycle::LifecycleError;
use crate::ratatui_node::runtime::RemoteNodeConnectionInfo;
use crate::ratatui_node::runtime::RemoteNodeConnectionMode;
use crate::ratatui_node::state_event::StateEvent;
use crate::ratatui_node::SharedState;
use crate::remote::publication::remote_target_publication_backend::{
    RemoteTargetPublicationBackend, RemoteTargetPublicationBinding,
};
use std::path::Path;
use std::sync::Arc;

/// Ratatui-backed implementation of `RemoteTargetPublicationBackend`.
///
/// In the single-process ratatui server model, sessions live in an in-memory
/// `SharedState` catalog and the TUI receives snapshots broadcast from the
/// server. Hooks, sidecars, and legacy socket operations are no-ops here.
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
        let guard = self
            .shared
            .sessions
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner());
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
        let guard = self
            .shared
            .sessions
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner());
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
        // Self-referential guard: when the authority (control host) shares this
        // node's advertised node id, the rewrite above maps the authority's own
        // session id onto this node's node-owned session.  Such sessions are
        // created locally and never requested by a remote viewer (`opened_by`
        // is empty), so a remote TargetExited for them describes the
        // authority's session, not ours.  Closing ours here would kill a live
        // node-owned session, so the signal is ignored.  The sessions lock is
        // released before any event is sent.
        let is_node_owned_local_session = {
            let guard = self
                .shared
                .sessions
                .sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            guard.get(&local_target).is_some_and(|record| {
                record.address.authority_id() == self.shared.local_authority_id()
                    && record.opened_by.is_empty()
            })
        };
        if is_node_owned_local_session {
            ERROR_LOG.log(format!(
                "[ratatui-node] ignoring remote TargetExited for node-owned session \
                 {local_target}: the signal describes the authority's own session"
            ));
            return Ok(());
        }
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

    fn signal_remote_node_online(&self, node_id: &str) -> Result<(), LifecycleError> {
        let _ = self
            .shared
            .state_sender()
            .send(StateEvent::RemoteNodeOnline {
                node_id: node_id.to_string(),
            });
        Ok(())
    }

    fn signal_remote_node_auth_rejected(
        &self,
        node_id: &str,
        message: &str,
    ) -> Result<(), LifecycleError> {
        let _ = self
            .shared
            .state_sender()
            .send(StateEvent::RemoteNodeAuthRejected {
                node_id: node_id.to_string(),
                message: message.to_string(),
            });
        Ok(())
    }

    fn record_inbound_remote_node_connection(
        &self,
        node_id: &str,
        host: &str,
        port: u16,
        server_can_reach_peer: bool,
    ) -> Result<(), LifecycleError> {
        let _ = self
            .shared
            .state_sender()
            .send(StateEvent::RecordRemoteNodeConnection {
                node_id: node_id.to_string(),
                info: RemoteNodeConnectionInfo {
                    mode: RemoteNodeConnectionMode::InboundConnect,
                    host: host.to_string(),
                    port,
                    tls_pin_sha256: String::new(),
                    profile_name: String::new(),
                    server_can_reach_peer,
                },
            });
        Ok(())
    }

    fn on_remote_session_upserted(
        &self,
        _node_id: &str,
        session: &ManagedSessionRecord,
    ) -> Result<(), LifecycleError> {
        let _ = self
            .shared
            .state_sender()
            .send(StateEvent::RemoteSessionCatalogUpdated {
                record: Box::new(session.clone()),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::session_catalog::{
        ConsoleAttachment, ConsoleLocation, ManagedSessionAddress, ManagedSessionTaskState,
        SessionAvailability,
    };
    use std::sync::mpsc;
    use std::time::Duration;

    fn test_network() -> RemoteNetworkConfig {
        // The authority runs on the same host and port as this node server, so
        // its advertised node id equals this node's advertised node id.
        RemoteNetworkConfig {
            port: 7474,
            node_id: Some("192.168.1.9#7474".to_string()),
            ..RemoteNetworkConfig::default()
        }
    }

    fn authority_host_record(opened_by: Vec<ConsoleAttachment>) -> ManagedSessionRecord {
        ManagedSessionRecord {
            address: ManagedSessionAddress::local("local#7474", "1"),
            selector: None,
            availability: SessionAvailability::Online,
            workspace_dir: None,
            workspace_key: None,
            session_role: Some(crate::domain::workspace::WorkspaceSessionRole::TargetHost),
            opened_by,
            attached_clients: 0,
            window_count: 1,
            command_name: Some("bash".to_string()),
            display_command_name: None,
            agent_command_name: None,
            current_path: None,
            task_state: ManagedSessionTaskState::Input,
        }
    }

    fn backend_with_record(
        record: Option<ManagedSessionRecord>,
    ) -> (
        RatatuiRemoteTargetPublicationBackend,
        mpsc::Receiver<StateEvent>,
    ) {
        let network = test_network();
        let shared = SharedState::new(network.clone()).expect("SharedState::new should succeed");
        let (tx, rx) = mpsc::channel();
        shared.set_state_tx(tx);
        if let Some(record) = record {
            let target_id = record.address.qualified_target();
            shared
                .sessions
                .sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(target_id, record);
        }
        (
            RatatuiRemoteTargetPublicationBackend::new(shared, network),
            rx,
        )
    }

    fn signal_target_exited(
        backend: &RatatuiRemoteTargetPublicationBackend,
    ) -> Result<(), LifecycleError> {
        backend.signal_remote_target_exited(
            "ratatui-7474",
            "",
            "remote-peer:192.168.1.9#7474:1",
            Path::new("/bin/true"),
        )
    }

    #[test]
    fn signal_remote_target_exited_ignores_node_owned_authority_host_session() {
        let (backend, rx) = backend_with_record(Some(authority_host_record(Vec::new())));

        signal_target_exited(&backend).expect("signal should succeed");

        assert!(
            rx.recv_timeout(Duration::from_millis(200)).is_err(),
            "no SessionClosed event should be sent for a node-owned session"
        );
    }

    #[test]
    fn signal_remote_target_exited_closes_viewer_owned_session() {
        let opened_by = vec![ConsoleAttachment {
            console_id: "viewer-1".to_string(),
            location: ConsoleLocation::ServerConsole,
            has_pty_resize_authority: true,
        }];
        let (backend, rx) = backend_with_record(Some(authority_host_record(opened_by)));

        signal_target_exited(&backend).expect("signal should succeed");

        match rx
            .recv_timeout(Duration::from_millis(200))
            .expect("SessionClosed should be sent for a viewer-owned session")
        {
            StateEvent::SessionClosed { target_id } => {
                assert_eq!(target_id, "local#7474:1");
            }
            other => panic!("expected SessionClosed for viewer-owned session, got {other:?}"),
        }
    }

    #[test]
    fn signal_remote_target_exited_keeps_behavior_for_unknown_target() {
        let (backend, rx) = backend_with_record(None);

        signal_target_exited(&backend).expect("signal should succeed");

        match rx
            .recv_timeout(Duration::from_millis(200))
            .expect("SessionClosed should be sent for an unknown target")
        {
            StateEvent::SessionClosed { target_id } => {
                assert_eq!(target_id, "local#7474:1");
            }
            other => panic!("expected SessionClosed for unknown target, got {other:?}"),
        }
    }
}
