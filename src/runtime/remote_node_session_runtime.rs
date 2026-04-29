use crate::domain::session_catalog::ManagedSessionRecord;
use crate::domain::workspace::WorkspaceSessionRole;
use crate::infra::remote_protocol::{
    ControlPlanePayload, NodeSessionChannel, NodeSessionEnvelope, ProtocolEnvelope,
    TargetExitedPayload, TargetOutputPayload, TargetPublishedPayload, REMOTE_PROTOCOL_VERSION,
};
use crate::infra::remote_transport_codec::{
    read_control_plane_envelope, read_node_session_envelope, write_control_plane_envelope,
    write_node_session_envelope, write_registration_frame, RemoteTransportCodecError,
};
use crate::runtime::remote_authority_connection_runtime::QueuedAuthorityStreamSink;
use crate::runtime::remote_authority_transport_runtime::RemoteAuthorityCommand;
use crate::runtime::remote_node_transport_runtime::{
    read_client_hello, read_server_hello, write_client_hello, write_server_hello,
};
use crate::runtime::remote_target_publication_runtime::PublicationSenderCommand;
use std::fmt;
use std::fs;
use std::io;
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

const NODE_SESSION_SERVER_ID: &str = "waitagent-remote-node-session";

pub trait RemoteNodePublicationSink: Send + Sync + 'static {
    fn publish(
        &self,
        envelope: ProtocolEnvelope<ControlPlanePayload>,
    ) -> Result<(), RemoteNodeSessionError>;
}

pub struct RemoteNodeSessionRuntime {
    node_id: String,
    reader: Mutex<UnixStream>,
    writer: Mutex<UnixStream>,
    next_message_id: AtomicU64,
}

pub struct RemoteNodeSessionListenerGuard {
    socket_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteNodeSessionError {
    message: String,
}

impl RemoteNodeSessionRuntime {
    pub fn connect(
        socket_path: impl AsRef<Path>,
        node_id: impl Into<String>,
    ) -> Result<Self, RemoteNodeSessionError> {
        let node_id = node_id.into();
        let mut stream = UnixStream::connect(socket_path)?;
        write_client_hello(&mut stream, &node_id)?;
        let _server_hello = read_server_hello(&mut stream)?;
        let writer = stream.try_clone()?;
        Ok(Self {
            node_id,
            reader: Mutex::new(stream),
            writer: Mutex::new(writer),
            next_message_id: AtomicU64::new(0),
        })
    }

    pub fn recv_authority_command(&self) -> Result<RemoteAuthorityCommand, RemoteNodeSessionError> {
        let mut reader = self
            .reader
            .lock()
            .expect("node session reader mutex should not be poisoned");
        let session_envelope = read_node_session_envelope(&mut *reader)?;
        if session_envelope.channel != NodeSessionChannel::Authority {
            return Err(RemoteNodeSessionError::new(format!(
                "unexpected node session channel `{}` while waiting for authority command",
                session_envelope.channel.as_str()
            )));
        }
        match session_envelope.envelope.payload {
            ControlPlanePayload::TargetInput(payload) => {
                Ok(RemoteAuthorityCommand::TargetInput(payload))
            }
            ControlPlanePayload::ApplyResize(payload) => {
                Ok(RemoteAuthorityCommand::ApplyResize(payload))
            }
            other => Err(RemoteNodeSessionError::new(format!(
                "unexpected authority command `{}`",
                other.message_type()
            ))),
        }
    }

    pub fn send_target_output(
        &self,
        target_id: &str,
        output_seq: u64,
        stream: &'static str,
        bytes_base64: impl Into<String>,
    ) -> Result<(), RemoteNodeSessionError> {
        self.send_payload(
            NodeSessionChannel::Authority,
            target_id,
            "authority-msg",
            ControlPlanePayload::TargetOutput(TargetOutputPayload {
                target_id: target_id.to_string(),
                output_seq,
                stream,
                bytes_base64: bytes_base64.into(),
            }),
        )
    }

    pub fn send_target_published(
        &self,
        target: &ManagedSessionRecord,
        source_session_name: Option<&str>,
    ) -> Result<(), RemoteNodeSessionError> {
        let current_path = target
            .current_path
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned());
        self.send_payload(
            NodeSessionChannel::Publication,
            target.address.id().as_str(),
            "publication-msg",
            ControlPlanePayload::TargetPublished(TargetPublishedPayload {
                transport_session_id: target.address.session_id().to_string(),
                source_session_name: source_session_name.map(str::to_string),
                selector: target.selector.clone(),
                availability: target.availability.as_str(),
                session_role: target
                    .session_role
                    .as_ref()
                    .map(WorkspaceSessionRole::as_str),
                workspace_key: target.workspace_key.clone(),
                command_name: target.command_name.clone(),
                current_path,
                attached_clients: target.attached_clients,
                window_count: target.window_count,
            }),
        )
    }

    pub fn send_target_exited(
        &self,
        transport_session_id: &str,
        source_session_name: Option<&str>,
    ) -> Result<(), RemoteNodeSessionError> {
        let target_id = format!("remote-peer:{}:{transport_session_id}", self.node_id);
        self.send_payload(
            NodeSessionChannel::Publication,
            &target_id,
            "publication-msg",
            ControlPlanePayload::TargetExited(TargetExitedPayload {
                transport_session_id: transport_session_id.to_string(),
                source_session_name: source_session_name.map(str::to_string),
            }),
        )
    }

    pub(crate) fn send_publication_sender_command(
        &self,
        command: &PublicationSenderCommand,
    ) -> Result<(), RemoteNodeSessionError> {
        match command {
            PublicationSenderCommand::PublishTarget {
                authority_id,
                transport_session_id,
                source_session_name,
                selector,
                availability,
                session_role,
                workspace_key,
                command_name,
                current_path,
                attached_clients,
                window_count,
            } => self.send_payload(
                NodeSessionChannel::Publication,
                &format!("remote-peer:{authority_id}:{transport_session_id}"),
                "publication-msg",
                ControlPlanePayload::TargetPublished(TargetPublishedPayload {
                    transport_session_id: transport_session_id.clone(),
                    source_session_name: source_session_name.clone(),
                    selector: selector.clone(),
                    availability,
                    session_role: *session_role,
                    workspace_key: workspace_key.clone(),
                    command_name: command_name.clone(),
                    current_path: current_path.clone(),
                    attached_clients: *attached_clients,
                    window_count: *window_count,
                }),
            ),
            PublicationSenderCommand::ExitTarget {
                authority_id,
                transport_session_id,
                source_session_name,
            } => self.send_payload(
                NodeSessionChannel::Publication,
                &format!("remote-peer:{authority_id}:{transport_session_id}"),
                "publication-msg",
                ControlPlanePayload::TargetExited(TargetExitedPayload {
                    transport_session_id: transport_session_id.clone(),
                    source_session_name: source_session_name.clone(),
                }),
            ),
            PublicationSenderCommand::RegisterLiveSession { .. }
            | PublicationSenderCommand::UnregisterLiveSession { .. } => {
                Err(RemoteNodeSessionError::new(
                    "live session registration commands cannot be sent over the node session",
                ))
            }
        }
    }

    pub fn shutdown(&self) {
        let _ = self
            .reader
            .lock()
            .expect("node session reader mutex should not be poisoned")
            .shutdown(Shutdown::Both);
        let _ = self
            .writer
            .lock()
            .expect("node session writer mutex should not be poisoned")
            .shutdown(Shutdown::Both);
    }

    fn send_payload(
        &self,
        channel: NodeSessionChannel,
        target_id: &str,
        message_scope: &str,
        payload: ControlPlanePayload,
    ) -> Result<(), RemoteNodeSessionError> {
        let envelope = ProtocolEnvelope {
            protocol_version: REMOTE_PROTOCOL_VERSION.to_string(),
            message_id: format!(
                "{}-{}-{}",
                self.node_id,
                message_scope,
                self.next_message_id.fetch_add(1, Ordering::Relaxed) + 1
            ),
            message_type: payload.message_type(),
            timestamp: now_rfc3339_like(),
            sender_id: self.node_id.clone(),
            correlation_id: None,
            target_id: Some(target_id.to_string()),
            attachment_id: None,
            console_id: None,
            payload,
        };
        let mut writer = self
            .writer
            .lock()
            .expect("node session writer mutex should not be poisoned");
        write_node_session_envelope(&mut *writer, &NodeSessionEnvelope { channel, envelope })?;
        Ok(())
    }
}

pub fn spawn_remote_node_session_listener(
    socket_path: PathBuf,
    authority_sink: QueuedAuthorityStreamSink,
    publication_sink: Arc<dyn RemoteNodePublicationSink>,
) -> io::Result<RemoteNodeSessionListenerGuard> {
    if socket_path.exists() {
        let _ = fs::remove_file(&socket_path);
    }
    let listener = UnixListener::bind(&socket_path)?;
    thread::spawn(move || {
        for accepted in listener.incoming() {
            let Ok(stream) = accepted else {
                break;
            };
            let authority_sink = authority_sink.clone();
            let publication_sink = publication_sink.clone();
            thread::spawn(move || {
                let _ = bridge_remote_node_session(stream, authority_sink, publication_sink);
            });
        }
    });
    Ok(RemoteNodeSessionListenerGuard { socket_path })
}

impl RemoteNodeSessionError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for RemoteNodeSessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for RemoteNodeSessionError {}

impl From<io::Error> for RemoteNodeSessionError {
    fn from(value: io::Error) -> Self {
        Self::new(value.to_string())
    }
}

impl From<RemoteTransportCodecError> for RemoteNodeSessionError {
    fn from(value: RemoteTransportCodecError) -> Self {
        Self::new(value.to_string())
    }
}

impl Drop for RemoteNodeSessionListenerGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.socket_path);
    }
}

fn bridge_remote_node_session(
    mut transport_stream: UnixStream,
    authority_sink: QueuedAuthorityStreamSink,
    publication_sink: Arc<dyn RemoteNodePublicationSink>,
) -> Result<(), RemoteNodeSessionError> {
    let node_id = read_client_hello(&mut transport_stream)?;

    let (mut local_reader, pane_stream) = UnixStream::pair()?;
    authority_sink
        .submit(pane_stream)
        .map_err(|_| RemoteNodeSessionError::new("authority stream consumer is unavailable"))?;
    write_registration_frame(&mut local_reader, &node_id)?;

    let mut transport_writer = transport_stream.try_clone()?;
    write_server_hello(&mut transport_writer, NODE_SESSION_SERVER_ID)?;
    let local_writer = local_reader.try_clone()?;
    let authority_forward =
        thread::spawn(move || forward_authority_from_local(local_reader, transport_writer));
    let network_result = forward_network_session(local_writer, transport_stream, publication_sink);
    let _ = authority_forward.join();
    network_result
}

fn forward_authority_from_local(
    mut reader: UnixStream,
    mut writer: UnixStream,
) -> Result<(), RemoteNodeSessionError> {
    while let Ok(envelope) = read_control_plane_envelope(&mut reader) {
        write_node_session_envelope(
            &mut writer,
            &NodeSessionEnvelope {
                channel: NodeSessionChannel::Authority,
                envelope,
            },
        )?;
    }
    Ok(())
}

fn forward_network_session(
    mut authority_writer: UnixStream,
    mut transport_reader: UnixStream,
    publication_sink: Arc<dyn RemoteNodePublicationSink>,
) -> Result<(), RemoteNodeSessionError> {
    while let Ok(session_envelope) = read_node_session_envelope(&mut transport_reader) {
        match session_envelope.channel {
            NodeSessionChannel::Authority => {
                write_control_plane_envelope(&mut authority_writer, &session_envelope.envelope)?;
            }
            NodeSessionChannel::Publication => {
                publication_sink.publish(session_envelope.envelope)?;
            }
        }
    }
    Ok(())
}

fn now_rfc3339_like() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!("{millis}Z")
}

#[cfg(test)]
mod tests {
    use super::{
        spawn_remote_node_session_listener, RemoteNodePublicationSink, RemoteNodeSessionRuntime,
    };
    use crate::domain::session_catalog::{
        ManagedSessionAddress, ManagedSessionRecord, ManagedSessionTaskState, SessionAvailability,
    };
    use crate::domain::workspace::WorkspaceSessionRole;
    use crate::infra::remote_protocol::{
        ControlPlanePayload, ProtocolEnvelope, TargetInputPayload, TargetPublishedPayload,
    };
    use crate::runtime::remote_authority_connection_runtime::{
        AuthorityConnectionRequest, AuthorityConnectionStarter, AuthorityTransportEvent,
        QueuedAuthorityStreamStarter,
    };
    use crate::runtime::remote_transport_runtime::RemoteConnectionRegistry;
    use std::path::PathBuf;
    use std::sync::{mpsc, Arc, Mutex};
    use std::thread;
    use std::time::Duration;

    const TEST_TIMEOUT: Duration = Duration::from_secs(2);

    struct RecordingPublicationSink {
        tx: Mutex<mpsc::Sender<ProtocolEnvelope<ControlPlanePayload>>>,
    }

    impl RemoteNodePublicationSink for RecordingPublicationSink {
        fn publish(
            &self,
            envelope: ProtocolEnvelope<ControlPlanePayload>,
        ) -> Result<(), super::RemoteNodeSessionError> {
            self.tx
                .lock()
                .expect("publication sink mutex should not be poisoned")
                .send(envelope)
                .map_err(|_| super::RemoteNodeSessionError::new("publication test receiver closed"))
        }
    }

    #[test]
    fn single_outer_session_carries_authority_and_publication() {
        let socket_path = test_socket_path("mixed");
        let (authority_starter, authority_sink) = QueuedAuthorityStreamStarter::channel();
        let (tx, rx) = mpsc::channel();
        let publication_sink: Arc<dyn RemoteNodePublicationSink> =
            Arc::new(RecordingPublicationSink { tx: Mutex::new(tx) });
        let registry = RemoteConnectionRegistry::new();
        let (event_tx, event_rx) = mpsc::channel();
        let _guard = authority_starter
            .start_connection(
                AuthorityConnectionRequest {
                    socket_path: PathBuf::from("/tmp/unused.sock"),
                    authority_id: "peer-a".to_string(),
                },
                registry.clone(),
                event_tx,
            )
            .expect("authority starter should start");
        let _listener = spawn_remote_node_session_listener(
            socket_path.clone(),
            authority_sink,
            publication_sink,
        )
        .expect("node session listener should bind");

        let session = Arc::new(
            RemoteNodeSessionRuntime::connect(&socket_path, "peer-a")
                .expect("node session should connect"),
        );

        let session_writer = session.clone();
        let authority_thread = thread::spawn(move || {
            while let Ok(event) = event_rx.recv_timeout(TEST_TIMEOUT) {
                if let AuthorityTransportEvent::Connected = event {
                    break;
                }
            }
            let connection = registry
                .connection_for("peer-a")
                .expect("authority connection should register");
            connection
                .send(&ProtocolEnvelope {
                    protocol_version: crate::infra::remote_protocol::REMOTE_PROTOCOL_VERSION
                        .to_string(),
                    message_id: "msg-in".to_string(),
                    message_type: "target_input",
                    timestamp: "1Z".to_string(),
                    sender_id: "observer".to_string(),
                    correlation_id: None,
                    target_id: Some("remote-peer:peer-a:shell-1".to_string()),
                    attachment_id: Some("att-1".to_string()),
                    console_id: Some("console-1".to_string()),
                    payload: ControlPlanePayload::TargetInput(TargetInputPayload {
                        attachment_id: "att-1".to_string(),
                        target_id: "remote-peer:peer-a:shell-1".to_string(),
                        console_id: "console-1".to_string(),
                        console_host_id: "wa-local".to_string(),
                        input_seq: 1,
                        bytes_base64: "YQ==".to_string(),
                    }),
                })
                .expect("authority input should send");
            session_writer
                .send_target_published(&remote_target("peer-a", "shell-1"), Some("target-host-1"))
                .expect("publication should send");
        });

        match session
            .recv_authority_command()
            .expect("authority command should arrive")
        {
            crate::runtime::remote_authority_transport_runtime::RemoteAuthorityCommand::TargetInput(
                payload,
            ) => {
                assert_eq!(payload.target_id, "remote-peer:peer-a:shell-1");
            }
            other => panic!("unexpected authority command: {other:?}"),
        }
        authority_thread
            .join()
            .expect("authority helper thread should join cleanly");
        let envelope = rx
            .recv_timeout(TEST_TIMEOUT)
            .expect("publication envelope should arrive");
        match envelope.payload {
            ControlPlanePayload::TargetPublished(TargetPublishedPayload {
                transport_session_id,
                source_session_name,
                ..
            }) => {
                assert_eq!(transport_session_id, "shell-1");
                assert_eq!(source_session_name.as_deref(), Some("target-host-1"));
            }
            other => panic!("unexpected publication payload: {other:?}"),
        }
    }

    fn remote_target(authority_id: &str, session_id: &str) -> ManagedSessionRecord {
        ManagedSessionRecord {
            address: ManagedSessionAddress::remote_peer(authority_id, session_id),
            selector: Some("wk:shell".to_string()),
            availability: SessionAvailability::Online,
            workspace_dir: None,
            workspace_key: Some("wk-1".to_string()),
            session_role: Some(WorkspaceSessionRole::TargetHost),
            opened_by: Vec::new(),
            attached_clients: 2,
            window_count: 1,
            command_name: Some("codex".to_string()),
            current_path: Some(PathBuf::from("/tmp/demo")),
            task_state: ManagedSessionTaskState::Unknown,
        }
    }

    fn test_socket_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "waitagent-remote-node-session-test-{}-{}.sock",
            std::process::id(),
            label
        ))
    }
}
