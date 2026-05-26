use crate::application::remote_control_plane_service::RemoteControlPlaneService;
use crate::domain::session_catalog::ManagedSessionRecord;
use crate::infra::error_log::ERROR_LOG;
use crate::infra::remote_protocol::{
    ControlPlaneDestination, ControlPlanePayload, NodeBoundControlPlaneMessage,
    RemoteConsoleDescriptor, RoutedControlPlaneMessage,
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
    pub session_id: String,
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
    #[cfg(test)]
    pub fn new(sink: Box<dyn RemoteControlPlaneSink>) -> Self {
        Self {
            control_plane: RefCell::new(RemoteControlPlaneService::new()),
            sink,
            connection_registry: None,
        }
    }

    #[cfg(test)]
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

    pub fn record_mirror_accepted(&self, session_id: &str) {
        self.control_plane
            .borrow_mut()
            .record_mirror_accepted(session_id);
    }

    pub fn record_mirror_rejected(&self, session_id: &str, reason: String) {
        self.control_plane
            .borrow_mut()
            .record_mirror_rejected(session_id, reason);
    }

    pub fn handle_authority_disconnect(&self, authority_node_id: &str) {
        self.control_plane
            .borrow_mut()
            .handle_authority_disconnect(authority_node_id);
    }

    #[cfg(test)]
    pub fn activate_target(
        &self,
        target: &ManagedSessionRecord,
        console: RemoteConsoleDescriptor,
        cols: usize,
        rows: usize,
    ) -> Result<RemoteAttachmentBinding, LifecycleError> {
        self.activate_target_with_raw_pty_mode(target, console, cols, rows, false)
    }

    pub fn activate_target_with_raw_pty_mode(
        &self,
        target: &ManagedSessionRecord,
        console: RemoteConsoleDescriptor,
        cols: usize,
        rows: usize,
        raw_pty_passthrough: bool,
    ) -> Result<RemoteAttachmentBinding, LifecycleError> {
        let messages = {
            self.control_plane
                .borrow_mut()
                .open_target_with_raw_pty_mode(target, console, cols, rows, raw_pty_passthrough)
                .map_err(|error| LifecycleError::Protocol(error.to_string()))?
        };
        let binding = extract_open_binding(&messages).ok_or_else(|| {
            LifecycleError::Protocol(
                "remote open_target did not yield an open_target_ok attachment".to_string(),
            )
        })?;

        // Split messages: observer-bound (must succeed, local mailbox) vs
        // authority-bound (may fail if sidecar hasn't connected yet).
        let session_id = target.address.session_id().to_string();
        let (observer_msgs, authority_msgs): (Vec<_>, Vec<_>) =
            messages.into_iter().partition(|msg| {
                !matches!(&msg.destination, ControlPlaneDestination::AuthorityNode(_))
            });

        // Observer messages go through local LoopbackConnection — must succeed.
        {
            let deliveries = self
                .control_plane
                .borrow()
                .resolve_node_deliveries(&observer_msgs)
                .map_err(|error| LifecycleError::Protocol(error.to_string()))?;
            self.sink
                .send(&deliveries)
                .map_err(|error| LifecycleError::Protocol(error.to_string()))?;
        }

        // Authority messages may fail if the sidecar authority transport
        // isn't connected yet.  That's fine — replay content is already
        // en route to the observer, and mirror_pending will be retried
        // when the authority transport connects.
        if !authority_msgs.is_empty() {
            if let Err(error) = (|| -> Result<(), RemoteControlPlaneTransportError> {
                let deliveries = self
                    .control_plane
                    .borrow()
                    .resolve_node_deliveries(&authority_msgs)
                    .map_err(|e| RemoteControlPlaneTransportError::new(e.to_string()))?;
                self.sink.send(&deliveries)?;
                Ok(())
            })() {
                let _ = self
                    .control_plane
                    .borrow_mut()
                    .clear_mirror_pending(&session_id);
                log_diagnostic(format_args!(
                    "authority not ready, deferred (will retry on connect): {error}"
                ));
            }
        }

        Ok(binding)
    }

    /// Used by the authority connect handler to check whether a mirror
    /// request needs to be (re-)sent.
    pub fn is_mirror_pending(&self, target: &ManagedSessionRecord) -> bool {
        let session_id = target.address.session_id().to_string();
        self.control_plane.borrow().is_mirror_pending(&session_id)
    }

    /// Returns true when the session has no mirror request in flight or active
    /// (mirror_route is None). After failed authority delivery the pending state
    /// gets cleared back to None, so the connect handler must retry.
    pub fn is_mirror_needed(&self, target: &ManagedSessionRecord) -> bool {
        let session_id = target.address.session_id().to_string();
        self.control_plane.borrow().is_mirror_needed(&session_id)
    }

    pub fn send_raw_pty_input(
        &self,
        target: &ManagedSessionRecord,
        binding: &RemoteAttachmentBinding,
        console_seq: u64,
        input_bytes: Vec<u8>,
    ) -> Result<(), LifecycleError> {
        let message = self
            .control_plane
            .borrow_mut()
            .route_raw_pty_input(target, &binding.attachment_id, console_seq, input_bytes)
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

    pub fn close_target(
        &self,
        target: &ManagedSessionRecord,
        binding: &RemoteAttachmentBinding,
    ) -> Result<(), LifecycleError> {
        let messages = self
            .control_plane
            .borrow_mut()
            .close_target(target, &binding.attachment_id)
            .map_err(|error| LifecycleError::Protocol(error.to_string()))?;
        if messages.is_empty() {
            return Ok(());
        }
        self.send_messages(&messages)
    }

    pub fn send_target_output(
        &self,
        target: &ManagedSessionRecord,
        output_seq: u64,
        stream: &'static str,
        output_bytes: Vec<u8>,
    ) -> Result<(), LifecycleError> {
        let byte_count = output_bytes.len();
        let message = self
            .control_plane
            .borrow_mut()
            .route_target_output(target, output_seq, stream, output_bytes)
            .map_err(|error| LifecycleError::Protocol(error.to_string()))?;
        let result = self.send_messages(&[message]);
        match &result {
            Ok(()) => ERROR_LOG.log(format!(
                "[diag-timing] send_target_output: seq={} ({} bytes) delivered OK",
                output_seq, byte_count
            )),
            Err(e) => ERROR_LOG.log(format!(
                "[diag-timing] send_target_output: seq={} ({} bytes) FAILED: {e}",
                output_seq, byte_count
            )),
        }
        result
    }

    pub fn send_raw_pty_output(
        &self,
        target: &ManagedSessionRecord,
        output_seq: u64,
        output_bytes: Vec<u8>,
    ) -> Result<(), LifecycleError> {
        let message = self
            .control_plane
            .borrow_mut()
            .route_raw_pty_output(target, output_seq, output_bytes)
            .map_err(|error| LifecycleError::Protocol(error.to_string()))?;
        self.send_messages(&[message])
    }

    pub fn send_mirror_bootstrap_chunk(
        &self,
        target: &ManagedSessionRecord,
        chunk_seq: u64,
        stream: &'static str,
        output_bytes: Vec<u8>,
    ) -> Result<(), LifecycleError> {
        let message = self
            .control_plane
            .borrow_mut()
            .route_mirror_bootstrap_chunk(target, chunk_seq, stream, output_bytes)
            .map_err(|error| LifecycleError::Protocol(error.to_string()))?;
        self.send_messages(&[message])
    }

    pub fn send_mirror_bootstrap_complete(
        &self,
        target: &ManagedSessionRecord,
        last_chunk_seq: u64,
        alternate_screen_active: bool,
        application_cursor_keys: bool,
        cursor_visible: bool,
    ) -> Result<(), LifecycleError> {
        let message = self
            .control_plane
            .borrow_mut()
            .route_mirror_bootstrap_complete(
                target,
                last_chunk_seq,
                alternate_screen_active,
                application_cursor_keys,
                cursor_visible,
            )
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
                session_id: payload.session_id.clone(),
                target_id: payload.target_id.clone(),
                attachment_id: payload.attachment_id.clone(),
                console_id: payload.console_id.clone(),
            }),
            _ => None,
        })
}

/// Buffered diagnostic logger for the main-slot process.
/// Writes timestamped lines to a unique file in the temp directory so that
/// diagnostic messages don't pollute the terminal display.
pub(super) fn log_diagnostic(msg: impl fmt::Display) {
    use std::fs::OpenOptions;
    use std::io::{BufWriter, Write};
    use std::sync::{Mutex, OnceLock};

    static LOG: OnceLock<Mutex<BufWriter<std::fs::File>>> = OnceLock::new();
    let writer = LOG.get_or_init(|| {
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("waitagent-diag-main-slot-{pid}.log"));
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .expect("failed to open main-slot diagnostic log file");
        Mutex::new(BufWriter::with_capacity(4096, file))
    });

    fn now_millis() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    let mut guard = writer.lock().unwrap();
    let _ = writeln!(guard, "[{}] {}", now_millis(), msg);
    // Flush on every write so logs survive a crash.
    let _ = guard.flush();
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
                session_id: "shell-1".to_string(),
                target_id: "remote-peer:peer-a:shell-1".to_string(),
                attachment_id: "attach-1".to_string(),
                console_id: "console-a".to_string(),
            }
        );

        let sent_messages = sent_messages.borrow();
        // Messages are now split: observer batch first, authority batch second.
        assert_eq!(sent_messages.len(), 2);
        // Observer batch: OpenTargetOk + ResizeAuthorityChanged → 2 deliveries
        assert_eq!(sent_messages[0].len(), 2);
        assert_eq!(sent_messages[0][0].node_id, "observer-a");
        assert_eq!(sent_messages[0][1].node_id, "observer-a");
        assert_eq!(sent_messages[0][0].envelope.message_type, "open_target_ok");
        match &sent_messages[0][1].envelope.payload {
            ControlPlanePayload::ResizeAuthorityChanged(payload) => {
                assert_eq!(payload.cols, None);
                assert_eq!(payload.rows, None);
            }
            other => panic!("unexpected payload: {other:?}"),
        }
        // Authority batch: OpenMirrorRequest → 1 delivery
        assert_eq!(sent_messages[1].len(), 1);
        assert_eq!(sent_messages[1][0].node_id, "peer-a");
        assert_eq!(
            sent_messages[1][0].envelope.message_type,
            "open_mirror_request"
        );
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
        runtime.ensure_local_connection("peer-a");

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
    fn registry_backed_runtime_routes_raw_pty_input_to_authority_mailbox() {
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
            .send_raw_pty_input(
                &remote_target("peer-a", "shell-1"),
                &binding,
                1,
                b"a".to_vec(),
            )
            .expect("raw PTY input should route to authority");

        let envelopes = authority_mailbox.snapshot();
        assert_eq!(envelopes.len(), 2);
        assert_eq!(envelopes[0].message_type, "open_mirror_request");
        assert_eq!(envelopes[1].message_type, "raw_pty_input");
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
        assert_eq!(envelopes.len(), 2);
        assert_eq!(envelopes[0].message_type, "open_mirror_request");
        assert_eq!(envelopes[1].message_type, "apply_resize");
    }

    #[test]
    fn close_target_routes_close_mirror_for_last_attachment() {
        let runtime = RemoteMainSlotRuntime::with_registry(RemoteConnectionRegistry::new());
        runtime.ensure_local_connection("observer-a");
        let authority_mailbox = runtime
            .ensure_local_connection("peer-a")
            .expect("registry-backed runtime should expose authority registration");

        let target = remote_target("peer-a", "shell-1");
        let binding = runtime
            .activate_target(&target, console("console-a", "observer-a"), 120, 40)
            .expect("remote activation should succeed");
        runtime
            .close_target(&target, &binding)
            .expect("closing the last attachment should succeed");

        let envelopes = authority_mailbox.snapshot();
        assert_eq!(envelopes.len(), 2);
        assert_eq!(envelopes[0].message_type, "open_mirror_request");
        assert_eq!(envelopes[1].message_type, "close_mirror_request");
    }

    #[test]
    fn authority_disconnect_allows_same_console_to_reopen_mirror() {
        let runtime = RemoteMainSlotRuntime::with_registry(RemoteConnectionRegistry::new());
        runtime.ensure_local_connection("observer-a");
        let authority_mailbox = runtime
            .ensure_local_connection("peer-a")
            .expect("registry-backed runtime should expose authority registration");
        let target = remote_target("peer-a", "shell-1");

        runtime
            .activate_target(&target, console("console-a", "observer-a"), 120, 40)
            .expect("first activation should succeed");
        runtime.record_mirror_accepted("shell-1");
        runtime.handle_authority_disconnect("peer-a");
        runtime
            .activate_target(&target, console("console-a", "observer-a"), 120, 40)
            .expect("reconnect activation should succeed");

        let mirror_requests = authority_mailbox
            .snapshot()
            .into_iter()
            .filter(|envelope| envelope.message_type == "open_mirror_request")
            .count();
        assert_eq!(mirror_requests, 2);
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
            .send_target_output(&remote_target("peer-a", "shell-1"), 7, "pty", b"a".to_vec())
            .expect("target output should fan out to observers");

        let envelopes = observer_mailbox.snapshot();
        assert_eq!(envelopes.len(), already_seen + 1);
        assert_eq!(
            envelopes.last().map(|envelope| envelope.message_type),
            Some("target_output")
        );
    }

    #[test]
    fn registry_backed_runtime_routes_bootstrap_messages_to_observer_mailbox() {
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
            .send_mirror_bootstrap_chunk(
                &remote_target("peer-a", "shell-1"),
                1,
                "pty",
                b"a".to_vec(),
            )
            .expect("bootstrap chunk should fan out to observers");
        runtime
            .send_mirror_bootstrap_complete(
                &remote_target("peer-a", "shell-1"),
                1,
                false,
                false,
                true,
            )
            .expect("bootstrap complete should fan out to observers");

        let envelopes = observer_mailbox.snapshot();
        assert_eq!(envelopes.len(), already_seen + 2);
        assert_eq!(
            envelopes[already_seen].message_type,
            "mirror_bootstrap_chunk"
        );
        assert_eq!(
            envelopes[already_seen + 1].message_type,
            "mirror_bootstrap_complete"
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
