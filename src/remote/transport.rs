// Legacy tmux-era transport runtime kept during the ratatui migration; most items are currently unused.

use crate::infra::remote_protocol::{
    ControlPlanePayload, NodeBoundControlPlaneMessage, ProtocolEnvelope, RawPtyInputPayload,
};
use crate::remote::main_slot::remote_main_slot_runtime::{
    RemoteControlPlaneSink, RemoteControlPlaneTransportError,
};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};

// TODO(cleanup): transitional remote code, kept for Phase 8 wiring.
#[allow(dead_code)]
pub trait RemoteControlPlaneConnection: Send + Sync {
    fn send(
        &self,
        envelope: &ProtocolEnvelope<ControlPlanePayload>,
    ) -> Result<(), RemoteControlPlaneTransportError>;

    fn send_raw_pty_input(
        &self,
        _payload: &RawPtyInputPayload,
    ) -> Result<(), RemoteControlPlaneTransportError> {
        Err(RemoteControlPlaneTransportError::new(
            "remote control-plane connection does not support raw PTY input frames",
        ))
    }
}

#[derive(Clone, Default)]
pub struct RemoteConnectionRegistry {
    connections: Arc<Mutex<HashMap<String, RegisteredRemoteConnection>>>,
    next_generation: Arc<AtomicU64>,
}

#[derive(Clone)]
struct RegisteredRemoteConnection {
    generation: u64,
    connection: Arc<dyn RemoteControlPlaneConnection>,
}

// TODO(cleanup): transitional remote code, kept for Phase 8 wiring.
#[allow(dead_code)]
impl RemoteConnectionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register_connection(
        &self,
        node_id: impl Into<String>,
        connection: Arc<dyn RemoteControlPlaneConnection>,
    ) {
        let _ = self.register_connection_with_generation(node_id, connection);
    }

    pub fn register_connection_with_generation(
        &self,
        node_id: impl Into<String>,
        connection: Arc<dyn RemoteControlPlaneConnection>,
    ) -> u64 {
        let generation = self.next_generation.fetch_add(1, Ordering::Relaxed) + 1;
        self.connections
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                node_id.into(),
                RegisteredRemoteConnection {
                    generation,
                    connection,
                },
            );
        generation
    }

    pub fn unregister_connection_generation(&self, node_id: &str, generation: u64) -> bool {
        let mut connections = self.connections.lock().unwrap_or_else(|e| e.into_inner());
        let Some(connection) = connections.get(node_id) else {
            return false;
        };
        if connection.generation != generation {
            return false;
        }
        connections.remove(node_id).is_some()
    }

    pub fn has_connection(&self, node_id: &str) -> bool {
        self.connections
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(node_id)
    }

    pub fn register_loopback_connection(&self, node_id: impl Into<String>) -> LocalNodeMailbox {
        let mailbox = LocalNodeMailbox::default();
        self.register_connection(
            node_id,
            Arc::new(LoopbackConnection {
                mailbox: mailbox.clone(),
            }),
        );
        mailbox
    }

    pub(crate) fn connection_for(
        &self,
        node_id: &str,
    ) -> Option<Arc<dyn RemoteControlPlaneConnection>> {
        self.connections
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(node_id)
            .map(|registered| registered.connection.clone())
    }
}

pub struct RegistryRemoteControlPlaneSink {
    registry: RemoteConnectionRegistry,
}

// TODO(cleanup): transitional remote code, kept for Phase 8 wiring.
#[allow(dead_code)]
impl RegistryRemoteControlPlaneSink {
    pub fn new(registry: RemoteConnectionRegistry) -> Self {
        Self { registry }
    }
}

impl RemoteControlPlaneSink for RegistryRemoteControlPlaneSink {
    fn send(
        &self,
        deliveries: &[NodeBoundControlPlaneMessage],
    ) -> Result<(), RemoteControlPlaneTransportError> {
        for delivery in deliveries {
            let Some(connection) = self.registry.connection_for(&delivery.node_id) else {
                return Err(RemoteControlPlaneTransportError::new(format!(
                    "remote control-plane connection for node `{}` is not registered",
                    delivery.node_id
                )));
            };
            connection.send(&delivery.envelope)?;
        }
        Ok(())
    }
}

#[derive(Clone, Default)]
// TODO(cleanup): transitional remote code, kept for Phase 8 wiring.
#[allow(dead_code)]
pub struct LocalNodeMailbox {
    inner: Arc<LocalNodeMailboxInner>,
}

// TODO(cleanup): transitional remote code, kept for Phase 8 wiring.
#[allow(dead_code)]
#[derive(Default)]
struct LocalNodeMailboxInner {
    envelopes: Mutex<Vec<ProtocolEnvelope<ControlPlanePayload>>>,
    changed: Condvar,
}

// TODO(cleanup): transitional remote code, kept for Phase 8 wiring.
#[allow(dead_code)]
impl LocalNodeMailbox {
    pub fn snapshot(&self) -> Vec<ProtocolEnvelope<ControlPlanePayload>> {
        self.inner
            .envelopes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn snapshot_from(&self, start: usize) -> Vec<ProtocolEnvelope<ControlPlanePayload>> {
        self.inner
            .envelopes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .skip(start)
            .cloned()
            .collect()
    }

    pub fn wait_for_growth(&self, previous_len: usize) {
        let mut envelopes = self
            .inner
            .envelopes
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        while envelopes.len() <= previous_len {
            envelopes = self
                .inner
                .changed
                .wait(envelopes)
                .unwrap_or_else(|e| e.into_inner());
        }
    }
}

// TODO(cleanup): transitional remote code, kept for Phase 8 wiring.
#[allow(dead_code)]
struct LoopbackConnection {
    mailbox: LocalNodeMailbox,
}

// TODO(cleanup): transitional remote code, kept for Phase 8 wiring.
#[allow(dead_code)]
impl RemoteControlPlaneConnection for LoopbackConnection {
    fn send(
        &self,
        envelope: &ProtocolEnvelope<ControlPlanePayload>,
    ) -> Result<(), RemoteControlPlaneTransportError> {
        let mut envelopes = self
            .mailbox
            .inner
            .envelopes
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        envelopes.push(envelope.clone());
        drop(envelopes);
        self.mailbox.inner.changed.notify_all();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        LocalNodeMailbox, RegistryRemoteControlPlaneSink, RemoteConnectionRegistry,
        RemoteControlPlaneConnection,
    };
    use crate::infra::remote_protocol::{
        ControlPlanePayload, NodeBoundControlPlaneMessage, ProtocolEnvelope,
    };
    use crate::remote::main_slot::remote_main_slot_runtime::RemoteControlPlaneSink;
    use std::sync::{Arc, Mutex};

    #[test]
    fn registry_sink_routes_messages_to_registered_node_connections() {
        let registry = RemoteConnectionRegistry::new();
        let sink = RegistryRemoteControlPlaneSink::new(registry.clone());
        let observer_a = Arc::new(CapturingConnection::default());
        let observer_b = Arc::new(CapturingConnection::default());
        registry.register_connection("observer-a", observer_a.clone());
        registry.register_connection("observer-b", observer_b.clone());

        sink.send(&[
            delivery("observer-a", "open_target_ok"),
            delivery("observer-b", "resize_authority_changed"),
        ])
        .expect("registered connections should receive deliveries");

        assert_eq!(
            observer_a.message_types(),
            vec!["open_target_ok".to_string()]
        );
        assert_eq!(
            observer_b.message_types(),
            vec!["resize_authority_changed".to_string()]
        );
    }

    #[test]
    fn registry_sink_reports_missing_connection_by_node_id() {
        let registry = RemoteConnectionRegistry::new();
        let sink = RegistryRemoteControlPlaneSink::new(registry);

        let error = sink
            .send(&[delivery("observer-a", "open_target_ok")])
            .expect_err("missing connections should fail cleanly");

        assert_eq!(
            error.to_string(),
            "remote control-plane connection for node `observer-a` is not registered"
        );
    }

    #[test]
    fn registry_tracks_connection_presence() {
        let registry = RemoteConnectionRegistry::new();
        assert!(!registry.has_connection("observer-a"));

        let generation = registry.register_connection_with_generation(
            "observer-a",
            Arc::new(CapturingConnection::default()),
        );
        assert!(registry.has_connection("observer-a"));
        assert!(registry.unregister_connection_generation("observer-a", generation));
        assert!(!registry.has_connection("observer-a"));
    }

    #[test]
    fn registry_ignores_stale_generation_unregisters() {
        let registry = RemoteConnectionRegistry::new();
        let first = registry.register_connection_with_generation(
            "observer-a",
            Arc::new(CapturingConnection::default()),
        );
        let second = registry.register_connection_with_generation(
            "observer-a",
            Arc::new(CapturingConnection::default()),
        );

        assert!(!registry.unregister_connection_generation("observer-a", first));
        assert!(registry.has_connection("observer-a"));
        assert!(registry.unregister_connection_generation("observer-a", second));
        assert!(!registry.has_connection("observer-a"));
    }

    #[test]
    fn registry_can_register_loopback_connection_mailbox() {
        let registry = RemoteConnectionRegistry::new();
        let sink = RegistryRemoteControlPlaneSink::new(registry.clone());
        let mailbox = registry.register_loopback_connection("observer-a");

        sink.send(&[delivery("observer-a", "open_target_ok")])
            .expect("loopback connection should receive deliveries");

        assert_eq!(
            mailbox_message_types(&mailbox),
            vec!["open_target_ok".to_string()]
        );
    }

    #[derive(Default)]
    struct CapturingConnection {
        envelopes: Mutex<Vec<ProtocolEnvelope<ControlPlanePayload>>>,
    }

    impl CapturingConnection {
        fn message_types(&self) -> Vec<String> {
            self.envelopes
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .map(|envelope| envelope.message_type.to_string())
                .collect()
        }
    }

    impl RemoteControlPlaneConnection for CapturingConnection {
        fn send(
            &self,
            envelope: &ProtocolEnvelope<ControlPlanePayload>,
        ) -> Result<
            (),
            crate::remote::main_slot::remote_main_slot_runtime::RemoteControlPlaneTransportError,
        > {
            self.envelopes
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(envelope.clone());
            Ok(())
        }
    }

    fn delivery(node_id: &str, message_type: &'static str) -> NodeBoundControlPlaneMessage {
        NodeBoundControlPlaneMessage {
            node_id: node_id.to_string(),
            envelope: ProtocolEnvelope {
                protocol_version: "1.1".to_string(),
                message_id: format!("msg-{message_type}"),
                message_type,
                timestamp: "0Z".to_string(),
                sender_id: "server".to_string(),
                correlation_id: None,
                session_id: Some("shell-1".to_string()),
                target_id: Some("remote-peer:peer-a:shell-1".to_string()),
                attachment_id: Some("attach-1".to_string()),
                console_id: Some("console-a".to_string()),
                payload: ControlPlanePayload::Error(crate::infra::remote_protocol::ErrorPayload {
                    code: "test",
                    message: "test".to_string(),
                    details: None,
                }),
            },
        }
    }

    fn mailbox_message_types(mailbox: &LocalNodeMailbox) -> Vec<String> {
        mailbox
            .snapshot()
            .iter()
            .map(|envelope| envelope.message_type.to_string())
            .collect()
    }
}
