use crate::infra::remote_protocol::{
    ControlPlanePayload, NodeBoundControlPlaneMessage, ProtocolEnvelope,
};
use crate::runtime::remote_main_slot_runtime::{
    RemoteControlPlaneSink, RemoteControlPlaneTransportError,
};
use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex};

pub trait RemoteControlPlaneConnection: Send + Sync {
    fn send(
        &self,
        envelope: &ProtocolEnvelope<ControlPlanePayload>,
    ) -> Result<(), RemoteControlPlaneTransportError>;
}

#[derive(Clone, Default)]
pub struct RemoteConnectionRegistry {
    connections: Arc<Mutex<HashMap<String, Arc<dyn RemoteControlPlaneConnection>>>>,
}

impl RemoteConnectionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register_connection(
        &self,
        node_id: impl Into<String>,
        connection: Arc<dyn RemoteControlPlaneConnection>,
    ) {
        self.connections
            .lock()
            .expect("remote connection registry mutex should not be poisoned")
            .insert(node_id.into(), connection);
    }

    pub fn unregister_connection(&self, node_id: &str) -> bool {
        self.connections
            .lock()
            .expect("remote connection registry mutex should not be poisoned")
            .remove(node_id)
            .is_some()
    }

    pub fn has_connection(&self, node_id: &str) -> bool {
        self.connections
            .lock()
            .expect("remote connection registry mutex should not be poisoned")
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
            .expect("remote connection registry mutex should not be poisoned")
            .get(node_id)
            .cloned()
    }
}

pub struct RegistryRemoteControlPlaneSink {
    registry: RemoteConnectionRegistry,
}

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
pub struct LocalNodeMailbox {
    inner: Arc<LocalNodeMailboxInner>,
}

#[derive(Default)]
struct LocalNodeMailboxInner {
    envelopes: Mutex<Vec<ProtocolEnvelope<ControlPlanePayload>>>,
    changed: Condvar,
}

impl LocalNodeMailbox {
    pub fn snapshot(&self) -> Vec<ProtocolEnvelope<ControlPlanePayload>> {
        self.inner
            .envelopes
            .lock()
            .expect("local observer mailbox mutex should not be poisoned")
            .clone()
    }

    pub fn snapshot_from(&self, start: usize) -> Vec<ProtocolEnvelope<ControlPlanePayload>> {
        self.inner
            .envelopes
            .lock()
            .expect("local observer mailbox mutex should not be poisoned")
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
            .expect("local observer mailbox mutex should not be poisoned");
        while envelopes.len() <= previous_len {
            envelopes = self
                .inner
                .changed
                .wait(envelopes)
                .expect("local observer mailbox mutex should not be poisoned");
        }
    }
}

struct LoopbackConnection {
    mailbox: LocalNodeMailbox,
}

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
            .expect("local observer mailbox mutex should not be poisoned");
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
    use crate::runtime::remote_main_slot_runtime::RemoteControlPlaneSink;
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

        registry.register_connection("observer-a", Arc::new(CapturingConnection::default()));
        assert!(registry.has_connection("observer-a"));
        assert!(registry.unregister_connection("observer-a"));
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
                .expect("capturing connection mutex should not be poisoned")
                .iter()
                .map(|envelope| envelope.message_type.to_string())
                .collect()
        }
    }

    impl RemoteControlPlaneConnection for CapturingConnection {
        fn send(
            &self,
            envelope: &ProtocolEnvelope<ControlPlanePayload>,
        ) -> Result<(), crate::runtime::remote_main_slot_runtime::RemoteControlPlaneTransportError>
        {
            self.envelopes
                .lock()
                .expect("capturing connection mutex should not be poisoned")
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
