use crate::infra::remote_protocol::{
    ApplyResizePayload, ControlPlanePayload, ProtocolEnvelope, TargetInputPayload,
    TargetOutputPayload, REMOTE_PROTOCOL_VERSION,
};
use crate::infra::remote_transport_codec::{
    read_control_plane_envelope, write_control_plane_envelope, write_registration_frame,
    RemoteTransportCodecError,
};
use crate::runtime::remote_authority_connection_runtime::QueuedAuthorityStreamSink;
use crate::runtime::remote_node_transport_runtime::{
    read_client_hello, read_server_hello, write_client_hello, write_server_hello,
};
use std::fmt;
use std::fs;
use std::io;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

const AUTHORITY_TRANSPORT_SERVER_ID: &str = "waitagent-main-slot";
pub struct RemoteAuthorityTransportRuntime {
    node_id: String,
    reader: Mutex<UnixStream>,
    writer: Mutex<UnixStream>,
    next_message_id: AtomicU64,
}

pub struct AuthorityTransportListenerGuard {
    socket_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteAuthorityCommand {
    TargetInput(TargetInputPayload),
    ApplyResize(ApplyResizePayload),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteAuthorityTransportError {
    message: String,
}

impl RemoteAuthorityTransportRuntime {
    pub fn connect(
        socket_path: impl AsRef<Path>,
        node_id: impl Into<String>,
    ) -> Result<Self, RemoteAuthorityTransportError> {
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

    pub fn recv_command(&self) -> Result<RemoteAuthorityCommand, RemoteAuthorityTransportError> {
        let mut reader = self
            .reader
            .lock()
            .expect("authority transport reader mutex should not be poisoned");
        let envelope = read_control_plane_envelope(&mut *reader)?;
        match envelope.payload {
            ControlPlanePayload::TargetInput(payload) => {
                Ok(RemoteAuthorityCommand::TargetInput(payload))
            }
            ControlPlanePayload::ApplyResize(payload) => {
                Ok(RemoteAuthorityCommand::ApplyResize(payload))
            }
            other => Err(RemoteAuthorityTransportError::new(format!(
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
    ) -> Result<(), RemoteAuthorityTransportError> {
        let payload = ControlPlanePayload::TargetOutput(TargetOutputPayload {
            target_id: target_id.to_string(),
            output_seq,
            stream,
            bytes_base64: bytes_base64.into(),
        });
        let envelope = ProtocolEnvelope {
            protocol_version: REMOTE_PROTOCOL_VERSION.to_string(),
            message_id: format!(
                "{}-authority-msg-{}",
                self.node_id,
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
            .expect("authority transport writer mutex should not be poisoned");
        write_control_plane_envelope(&mut *writer, &envelope)?;
        Ok(())
    }
}

pub fn spawn_authority_transport_listener(
    socket_path: PathBuf,
    sink: QueuedAuthorityStreamSink,
) -> io::Result<AuthorityTransportListenerGuard> {
    if socket_path.exists() {
        let _ = fs::remove_file(&socket_path);
    }
    let listener = UnixListener::bind(&socket_path)?;
    thread::spawn(move || {
        for accepted in listener.incoming() {
            let Ok(stream) = accepted else {
                break;
            };
            let sink = sink.clone();
            thread::spawn(move || {
                let _ = bridge_authority_transport(stream, sink);
            });
        }
    });
    Ok(AuthorityTransportListenerGuard { socket_path })
}

impl RemoteAuthorityTransportError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for RemoteAuthorityTransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for RemoteAuthorityTransportError {}

impl From<io::Error> for RemoteAuthorityTransportError {
    fn from(value: io::Error) -> Self {
        Self::new(value.to_string())
    }
}

impl From<RemoteTransportCodecError> for RemoteAuthorityTransportError {
    fn from(value: RemoteTransportCodecError) -> Self {
        Self::new(value.to_string())
    }
}

impl Drop for AuthorityTransportListenerGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.socket_path);
    }
}

pub fn authority_transport_socket_path(
    socket_name: &str,
    session_name: &str,
    target: &str,
) -> PathBuf {
    std::env::temp_dir().join(format!(
        "waitagent-remote-{}-{}-{}.sock",
        remote_transport_path_component(socket_name),
        remote_transport_path_component(session_name),
        remote_transport_path_component(target)
    ))
}

fn remote_transport_path_component(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn now_rfc3339_like() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!("{millis}Z")
}

fn bridge_authority_transport(
    mut transport_stream: UnixStream,
    sink: QueuedAuthorityStreamSink,
) -> Result<(), RemoteAuthorityTransportError> {
    let node_id = read_client_hello(&mut transport_stream)?;

    let (mut local_reader, pane_stream) = UnixStream::pair()?;
    sink.submit(pane_stream).map_err(|_| {
        RemoteAuthorityTransportError::new("authority stream consumer is unavailable")
    })?;
    write_registration_frame(&mut local_reader, &node_id)?;

    let mut transport_writer = transport_stream.try_clone()?;
    write_server_hello(&mut transport_writer, AUTHORITY_TRANSPORT_SERVER_ID)?;
    let local_writer = local_reader.try_clone()?;
    let forward_network =
        thread::spawn(move || forward_control_plane_envelopes(transport_stream, local_writer));
    let forward_local = forward_control_plane_envelopes(local_reader, transport_writer);
    let _ = forward_network.join();
    forward_local
}

fn forward_control_plane_envelopes(
    mut reader: UnixStream,
    mut writer: UnixStream,
) -> Result<(), RemoteAuthorityTransportError> {
    while let Ok(envelope) = read_control_plane_envelopes(&mut reader) {
        write_control_plane_envelope(&mut writer, &envelope)?;
    }
    Ok(())
}

fn read_control_plane_envelopes(
    reader: &mut UnixStream,
) -> Result<ProtocolEnvelope<ControlPlanePayload>, RemoteAuthorityTransportError> {
    read_control_plane_envelope(reader).map_err(RemoteAuthorityTransportError::from)
}

#[cfg(test)]
mod tests {
    use super::{
        authority_transport_socket_path, spawn_authority_transport_listener,
        RemoteAuthorityCommand, RemoteAuthorityTransportRuntime,
    };
    use crate::infra::remote_protocol::{
        ClientHelloPayload, ControlPlanePayload, ProtocolEnvelope, TargetInputPayload,
    };
    use crate::infra::remote_transport_codec::read_control_plane_envelope;
    use crate::runtime::remote_authority_connection_runtime::{
        AuthorityConnectionRequest, AuthorityTransportEvent, QueuedAuthorityStreamSource,
        RemoteAuthorityConnectionRuntime,
    };
    use crate::runtime::remote_main_slot_runtime::RemoteControlPlaneSink;
    use crate::runtime::remote_node_transport_runtime::{
        write_server_hello, NODE_TRANSPORT_CLIENT_VERSION,
    };
    use crate::runtime::remote_transport_runtime::{
        RegistryRemoteControlPlaneSink, RemoteConnectionRegistry,
    };
    use std::fs;
    use std::os::unix::net::UnixListener;
    use std::process;
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    #[test]
    fn authority_transport_socket_path_is_workspace_and_target_scoped() {
        let path = authority_transport_socket_path("wa-1", "workspace-1", "peer-a:shell-1");
        let rendered = path.to_string_lossy();

        assert!(rendered.contains("waitagent-remote-wa-1-workspace-1-peer-a_shell-1.sock"));
    }

    #[test]
    fn connect_sends_client_hello_and_accepts_server_hello() {
        let socket_path = test_socket_path("hello");
        let listener = UnixListener::bind(&socket_path).expect("listener should bind");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("listener should accept");
            let hello =
                read_control_plane_envelope(&mut stream).expect("client hello should decode");
            let sender_id = hello.sender_id.clone();
            let payload = match hello.payload {
                ControlPlanePayload::ClientHello(payload) => payload,
                other => panic!("unexpected hello payload: {other:?}"),
            };
            write_server_hello(&mut stream, "waitagent-main-slot")
                .expect("server hello should encode");
            (sender_id, payload)
        });

        let _runtime = RemoteAuthorityTransportRuntime::connect(&socket_path, "peer-a")
            .expect("authority runtime should connect");
        let (sender_id, payload) = server.join().expect("server should join cleanly");

        assert_eq!(sender_id, "peer-a");
        assert_eq!(
            payload,
            ClientHelloPayload {
                node_id: "peer-a".to_string(),
                client_version: NODE_TRANSPORT_CLIENT_VERSION.to_string(),
            }
        );
        let _ = fs::remove_file(&socket_path);
    }

    #[test]
    fn listener_bridges_authority_transport_into_registered_connection_runtime() {
        let socket_path = test_socket_path("bridge");
        let (source, sink) = QueuedAuthorityStreamSource::channel();
        let runtime = RemoteAuthorityConnectionRuntime::new(source);
        let registry = RemoteConnectionRegistry::new();
        let (tx, rx) = mpsc::channel();
        let _guard = runtime
            .start_connection_source(
                AuthorityConnectionRequest {
                    socket_path: test_socket_path("bridge-unused"),
                    authority_id: "peer-a".to_string(),
                },
                registry.clone(),
                tx,
            )
            .expect("queued authority runtime should start");
        let _listener = spawn_authority_transport_listener(socket_path.clone(), sink)
            .expect("authority transport listener should bind");

        let transport = RemoteAuthorityTransportRuntime::connect(&socket_path, "peer-a")
            .expect("authority runtime should connect");
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(1))
                .expect("connected event should arrive"),
            AuthorityTransportEvent::Connected
        );
        assert!(registry.has_connection("peer-a"));

        RegistryRemoteControlPlaneSink::new(registry.clone())
            .send(&[
                crate::infra::remote_protocol::NodeBoundControlPlaneMessage {
                    node_id: "peer-a".to_string(),
                    envelope: target_input_envelope(),
                },
            ])
            .expect("target input should route to bridged authority transport");
        assert_eq!(
            transport
                .recv_command()
                .expect("target input should decode"),
            RemoteAuthorityCommand::TargetInput(TargetInputPayload {
                attachment_id: "attach-1".to_string(),
                target_id: "remote-peer:peer-a:shell-1".to_string(),
                console_id: "console-a".to_string(),
                console_host_id: "observer-a".to_string(),
                input_seq: 7,
                bytes_base64: "YQ==".to_string(),
            })
        );

        transport
            .send_target_output("remote-peer:peer-a:shell-1", 11, "pty", "Yg==")
            .expect("target output should send");
        match rx
            .recv_timeout(Duration::from_secs(1))
            .expect("output envelope should arrive")
        {
            AuthorityTransportEvent::Envelope(envelope) => match envelope.payload {
                ControlPlanePayload::TargetOutput(payload) => {
                    assert_eq!(payload.output_seq, 11);
                    assert_eq!(payload.bytes_base64, "Yg==");
                }
                other => panic!("unexpected payload: {other:?}"),
            },
            other => panic!("unexpected event: {other:?}"),
        }
        let _ = fs::remove_file(&socket_path);
    }

    fn test_socket_path(name: &str) -> std::path::PathBuf {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        std::env::temp_dir().join(format!(
            "waitagent-test-remote-authority-{name}-{}-{millis}.sock",
            process::id()
        ))
    }

    fn target_input_envelope() -> ProtocolEnvelope<ControlPlanePayload> {
        ProtocolEnvelope {
            protocol_version: "1.1".to_string(),
            message_id: "msg-target-input".to_string(),
            message_type: "target_input",
            timestamp: "2026-04-28T00:00:00Z".to_string(),
            sender_id: "server".to_string(),
            correlation_id: None,
            target_id: Some("remote-peer:peer-a:shell-1".to_string()),
            attachment_id: Some("attach-1".to_string()),
            console_id: Some("console-a".to_string()),
            payload: ControlPlanePayload::TargetInput(TargetInputPayload {
                attachment_id: "attach-1".to_string(),
                target_id: "remote-peer:peer-a:shell-1".to_string(),
                console_id: "console-a".to_string(),
                console_host_id: "observer-a".to_string(),
                input_seq: 7,
                bytes_base64: "YQ==".to_string(),
            }),
        }
    }
}
