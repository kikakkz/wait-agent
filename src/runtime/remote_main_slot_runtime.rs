use crate::application::remote_control_plane_service::RemoteControlPlaneService;
use crate::domain::session_catalog::ManagedSessionRecord;
use crate::infra::remote_protocol::{
    ControlPlanePayload, NodeBoundControlPlaneMessage, RemoteConsoleDescriptor,
    RoutedControlPlaneMessage,
};
use crate::lifecycle::LifecycleError;
use crate::runtime::remote_transport_runtime::{
    LocalNodeMailbox, RegistryRemoteControlPlaneSink, RemoteConnectionRegistry,
};
use std::cell::RefCell;
use std::fmt;

pub trait RemoteControlPlaneSink {
    fn send(
        &self,
        deliveries: &[NodeBoundControlPlaneMessage],
    ) -> Result<(), RemoteControlPlaneTransportError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteControlPlaneTransportError {
    message: String,
}

impl RemoteControlPlaneTransportError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for RemoteControlPlaneTransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for RemoteControlPlaneTransportError {}

pub struct UnconfiguredRemoteControlPlaneSink;

impl RemoteControlPlaneSink for UnconfiguredRemoteControlPlaneSink {
    fn send(
        &self,
        _deliveries: &[NodeBoundControlPlaneMessage],
    ) -> Result<(), RemoteControlPlaneTransportError> {
        Err(RemoteControlPlaneTransportError::new(
            "remote control-plane transport is not configured for main-slot activation yet",
        ))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteAttachmentBinding {
    pub target_id: String,
    pub attachment_id: String,
    pub console_id: String,
}

pub struct RemoteMainSlotRuntime {
    control_plane: RefCell<RemoteControlPlaneService>,
    sink: Box<dyn RemoteControlPlaneSink>,
    connection_registry: Option<RemoteConnectionRegistry>,
}

impl RemoteMainSlotRuntime {
    pub fn new(sink: Box<dyn RemoteControlPlaneSink>) -> Self {
        Self {
            control_plane: RefCell::new(RemoteControlPlaneService::new()),
            sink,
            connection_registry: None,
        }
    }

    pub fn new_unconfigured() -> Self {
        Self::new(Box::new(UnconfiguredRemoteControlPlaneSink))
    }

    pub fn with_registry(connection_registry: RemoteConnectionRegistry) -> Self {
        Self {
            control_plane: RefCell::new(RemoteControlPlaneService::new()),
            sink: Box::new(RegistryRemoteControlPlaneSink::new(
                connection_registry.clone(),
            )),
            connection_registry: Some(connection_registry),
        }
    }

    pub fn ensure_local_observer_connection(
        &self,
        node_id: impl Into<String>,
    ) -> Option<LocalNodeMailbox> {
        self.ensure_local_connection(node_id)
    }

    pub fn ensure_local_connection(&self, node_id: impl Into<String>) -> Option<LocalNodeMailbox> {
        self.connection_registry
            .as_ref()
            .map(|registry| registry.register_loopback_connection(node_id))
    }

    pub fn has_connection(&self, node_id: &str) -> bool {
        self.connection_registry
            .as_ref()
            .map(|registry| registry.has_connection(node_id))
            .unwrap_or(false)
    }

    pub fn activate_target(
        &self,
        target: &ManagedSessionRecord,
        console: RemoteConsoleDescriptor,
        cols: usize,
        rows: usize,
    ) -> Result<RemoteAttachmentBinding, LifecycleError> {
        let messages = {
            self.control_plane
                .borrow_mut()
                .open_target(target, console, cols, rows)
                .map_err(|error| LifecycleError::Protocol(error.to_string()))?
        };
        let binding = extract_open_binding(&messages).ok_or_else(|| {
            LifecycleError::Protocol(
                "remote open_target did not yield an open_target_ok attachment".to_string(),
            )
        })?;
        self.send_messages(&messages)?;
        Ok(binding)
    }

    pub fn send_console_input(
        &self,
        target: &ManagedSessionRecord,
        binding: &RemoteAttachmentBinding,
        console_seq: u64,
        bytes_base64: impl Into<String>,
    ) -> Result<(), LifecycleError> {
        let message = self
            .control_plane
            .borrow_mut()
            .route_console_input(target, &binding.attachment_id, console_seq, bytes_base64)
            .map_err(|error| LifecycleError::Protocol(error.to_string()))?;
        self.send_messages(&[message])
    }

    pub fn send_pty_resize(
        &self,
        target: &ManagedSessionRecord,
        binding: &RemoteAttachmentBinding,
        cols: usize,
        rows: usize,
    ) -> Result<(), LifecycleError> {
        let message = self
            .control_plane
            .borrow_mut()
            .route_pty_resize_request(target, &binding.attachment_id, cols, rows)
            .map_err(|error| LifecycleError::Protocol(error.to_string()))?;
        self.send_messages(&[message])
    }

    pub fn send_target_output(
        &self,
        target: &ManagedSessionRecord,
        output_seq: u64,
        stream: &'static str,
        bytes_base64: impl Into<String>,
    ) -> Result<(), LifecycleError> {
        let message = self
            .control_plane
            .borrow_mut()
            .route_target_output(target, output_seq, stream, bytes_base64)
            .map_err(|error| LifecycleError::Protocol(error.to_string()))?;
        self.send_messages(&[message])
    }
}

impl RemoteMainSlotRuntime {
    fn send_messages(&self, messages: &[RoutedControlPlaneMessage]) -> Result<(), LifecycleError> {
        let deliveries = self
            .control_plane
            .borrow()
            .resolve_node_deliveries(messages)
            .map_err(|error| LifecycleError::Protocol(error.to_string()))?;
        self.sink
            .send(&deliveries)
            .map_err(|error| LifecycleError::Protocol(error.to_string()))
    }
}

fn extract_open_binding(messages: &[RoutedControlPlaneMessage]) -> Option<RemoteAttachmentBinding> {
    messages
        .iter()
        .find_map(|message| match &message.envelope.payload {
            ControlPlanePayload::OpenTargetOk(payload) => Some(RemoteAttachmentBinding {
                target_id: payload.target_id.clone(),
                attachment_id: payload.attachment_id.clone(),
                console_id: payload.console_id.clone(),
            }),
            _ => None,
        })
}

#[cfg(test)]
mod tests {
    use super::{
        RemoteAttachmentBinding, RemoteControlPlaneSink, RemoteControlPlaneTransportError,
        RemoteMainSlotRuntime,
    };
    use crate::domain::session_catalog::{
        ConsoleLocation, ManagedSessionAddress, ManagedSessionRecord, ManagedSessionTaskState,
        SessionAvailability,
    };
    use crate::infra::remote_protocol::{
        ControlPlanePayload, NodeBoundControlPlaneMessage, RemoteConsoleDescriptor,
    };
    use crate::runtime::remote_transport_runtime::RemoteConnectionRegistry;
    use std::cell::RefCell;
    use std::rc::Rc;

    #[test]
    fn activate_target_routes_open_messages_to_sink() {
        let sent_messages = Rc::new(RefCell::new(Vec::new()));
        let runtime = RemoteMainSlotRuntime::new(Box::new(CapturingSink {
            sent_messages: sent_messages.clone(),
        }));

        let binding = runtime
            .activate_target(
                &remote_target("peer-a", "shell-1"),
                console("console-a", "observer-a"),
                120,
                40,
            )
            .expect("remote activation should succeed");

        assert_eq!(
            binding,
            RemoteAttachmentBinding {
                target_id: "remote-peer:peer-a:shell-1".to_string(),
                attachment_id: "attach-1".to_string(),
                console_id: "console-a".to_string(),
            }
        );

        let sent_messages = sent_messages.borrow();
        assert_eq!(sent_messages.len(), 1);
        assert_eq!(sent_messages[0].len(), 2);
        assert_eq!(sent_messages[0][0].node_id, "observer-a");
        assert_eq!(sent_messages[0][1].node_id, "observer-a");
        match &sent_messages[0][1].envelope.payload {
            ControlPlanePayload::ResizeAuthorityChanged(payload) => {
                assert_eq!(payload.cols, None);
                assert_eq!(payload.rows, None);
            }
            other => panic!("unexpected payload: {other:?}"),
        }
    }

    #[test]
    fn activate_target_reports_missing_transport_configuration() {
        let runtime = RemoteMainSlotRuntime::new_unconfigured();

        let error = runtime
            .activate_target(
                &remote_target("peer-a", "shell-1"),
                console("console-a", "observer-a"),
                120,
                40,
            )
            .expect_err("unconfigured runtime should fail cleanly");

        assert_eq!(
            error.to_string(),
            "remote control-plane transport is not configured for main-slot activation yet"
        );
    }

    #[test]
    fn registry_backed_runtime_can_deliver_to_local_observer_mailbox() {
        let runtime = RemoteMainSlotRuntime::with_registry(RemoteConnectionRegistry::new());
        let mailbox = runtime
            .ensure_local_observer_connection("observer-a")
            .expect("registry-backed runtime should expose local observer registration");

        let binding = runtime
            .activate_target(
                &remote_target("peer-a", "shell-1"),
                console("console-a", "observer-a"),
                120,
                40,
            )
            .expect("registry-backed runtime should deliver observer messages");

        assert_eq!(binding.attachment_id, "attach-1");
        let envelopes = mailbox.snapshot();
        assert_eq!(envelopes.len(), 2);
        assert_eq!(envelopes[0].message_type, "open_target_ok");
        assert_eq!(envelopes[1].message_type, "resize_authority_changed");
    }

    #[test]
    fn registry_backed_runtime_routes_console_input_to_authority_mailbox() {
        let runtime = RemoteMainSlotRuntime::with_registry(RemoteConnectionRegistry::new());
        runtime.ensure_local_connection("observer-a");
        let authority_mailbox = runtime
            .ensure_local_connection("peer-a")
            .expect("registry-backed runtime should expose authority registration");

        let binding = runtime
            .activate_target(
                &remote_target("peer-a", "shell-1"),
                console("console-a", "observer-a"),
                120,
                40,
            )
            .expect("remote activation should succeed");
        runtime
            .send_console_input(&remote_target("peer-a", "shell-1"), &binding, 1, "YQ==")
            .expect("console input should route to authority");

        let envelopes = authority_mailbox.snapshot();
        assert_eq!(envelopes.len(), 1);
        assert_eq!(envelopes[0].message_type, "target_input");
    }

    #[test]
    fn registry_backed_runtime_routes_pty_resize_to_authority_mailbox() {
        let runtime = RemoteMainSlotRuntime::with_registry(RemoteConnectionRegistry::new());
        runtime.ensure_local_connection("observer-a");
        let authority_mailbox = runtime
            .ensure_local_connection("peer-a")
            .expect("registry-backed runtime should expose authority registration");

        let binding = runtime
            .activate_target(
                &remote_target("peer-a", "shell-1"),
                console("console-a", "observer-a"),
                120,
                40,
            )
            .expect("remote activation should succeed");
        runtime
            .send_pty_resize(&remote_target("peer-a", "shell-1"), &binding, 160, 60)
            .expect("PTY resize should route to authority");

        let envelopes = authority_mailbox.snapshot();
        assert_eq!(envelopes.len(), 1);
        assert_eq!(envelopes[0].message_type, "apply_resize");
    }

    #[test]
    fn registry_backed_runtime_routes_target_output_to_observer_mailbox() {
        let runtime = RemoteMainSlotRuntime::with_registry(RemoteConnectionRegistry::new());
        let observer_mailbox = runtime
            .ensure_local_connection("observer-a")
            .expect("registry-backed runtime should expose observer registration");
        runtime.ensure_local_connection("peer-a");

        runtime
            .activate_target(
                &remote_target("peer-a", "shell-1"),
                console("console-a", "observer-a"),
                120,
                40,
            )
            .expect("remote activation should succeed");
        let already_seen = observer_mailbox.snapshot().len();
        runtime
            .send_target_output(&remote_target("peer-a", "shell-1"), 7, "pty", "YQ==")
            .expect("target output should fan out to observers");

        let envelopes = observer_mailbox.snapshot();
        assert_eq!(envelopes.len(), already_seen + 1);
        assert_eq!(
            envelopes.last().map(|envelope| envelope.message_type),
            Some("target_output")
        );
    }

    #[derive(Clone)]
    struct CapturingSink {
        sent_messages: Rc<RefCell<Vec<Vec<NodeBoundControlPlaneMessage>>>>,
    }

    impl RemoteControlPlaneSink for CapturingSink {
        fn send(
            &self,
            deliveries: &[NodeBoundControlPlaneMessage],
        ) -> Result<(), RemoteControlPlaneTransportError> {
            self.sent_messages.borrow_mut().push(deliveries.to_vec());
            Ok(())
        }
    }

    fn console(console_id: &str, host_id: &str) -> RemoteConsoleDescriptor {
        RemoteConsoleDescriptor {
            console_id: console_id.to_string(),
            console_host_id: host_id.to_string(),
            location: ConsoleLocation::LocalWorkspace,
        }
    }

    fn remote_target(authority_id: &str, session_id: &str) -> ManagedSessionRecord {
        ManagedSessionRecord {
            address: ManagedSessionAddress::remote_peer(authority_id, session_id),
            selector: None,
            availability: SessionAvailability::Online,
            workspace_dir: None,
            workspace_key: None,
            session_role: None,
            opened_by: Vec::new(),
            attached_clients: 0,
            window_count: 1,
            command_name: Some("bash".to_string()),
            current_path: None,
            task_state: ManagedSessionTaskState::Running,
        }
    }
}
