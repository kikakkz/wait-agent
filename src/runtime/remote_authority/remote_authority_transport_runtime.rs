use crate::infra::remote_protocol::{
    ApplyResizePayload, CloseMirrorRequestPayload, ControlPlanePayload,
    MirrorBootstrapChunkPayload, MirrorBootstrapCompletePayload, OpenMirrorAcceptedPayload,
    OpenMirrorRejectedPayload, OpenMirrorRequestPayload, ProtocolEnvelope, RawPtyInputPayload,
    RawPtyOutputPayload, TargetOutputPayload, REMOTE_PROTOCOL_VERSION,
};
use crate::infra::remote_transport_codec::{
    read_authority_transport_frame, read_control_plane_envelope, write_authority_transport_frame,
    write_control_plane_envelope, write_registration_frame, AuthorityTransportFrame,
    RemoteTransportCodecError,
};
use crate::runtime::remote_authority_connection_runtime::QueuedAuthorityStreamSink;
use crate::runtime::remote_node_transport_runtime::{
    read_client_hello, read_server_hello, write_client_hello, write_server_hello,
};
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const AUTHORITY_TRANSPORT_READ_TIMEOUT: Duration = Duration::from_secs(15);
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
    OpenMirror(OpenMirrorRequestPayload),
    CloseMirror(CloseMirrorRequestPayload),
    RawPtyInput(RawPtyInputPayload),
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
        // Reader/Writer timeouts prevent indefinite blocking under network jitter:
        // the reader may be stranded during a gRPC reconnect window on the authority
        // node, and a slow or broken FIFO reader on the output-pump side must not
        // freeze the event loop by back-pressuring the transport writer.
        stream.set_read_timeout(Some(Duration::from_secs(15))).ok();
        writer.set_write_timeout(Some(Duration::from_secs(5))).ok();
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
            ControlPlanePayload::OpenMirrorRequest(payload) => {
                Ok(RemoteAuthorityCommand::OpenMirror(payload))
            }
            ControlPlanePayload::CloseMirrorRequest(payload) => {
                Ok(RemoteAuthorityCommand::CloseMirror(payload))
            }
            ControlPlanePayload::RawPtyInput(payload) => {
                Ok(RemoteAuthorityCommand::RawPtyInput(payload))
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
        session_id: &str,
        target_id: &str,
        output_seq: u64,
        stream: &'static str,
        output_bytes: Vec<u8>,
    ) -> Result<(), RemoteAuthorityTransportError> {
        let payload = ControlPlanePayload::TargetOutput(TargetOutputPayload {
            session_id: session_id.to_string(),
            target_id: target_id.to_string(),
            output_seq,
            stream,
            output_bytes,
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
            session_id: Some(session_id.to_string()),
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

    pub fn send_raw_pty_output(
        &self,
        session_id: &str,
        target_id: &str,
        output_seq: u64,
        output_bytes: Vec<u8>,
    ) -> Result<(), RemoteAuthorityTransportError> {
        let payload = ControlPlanePayload::RawPtyOutput(RawPtyOutputPayload {
            session_id: session_id.to_string(),
            target_id: target_id.to_string(),
            output_seq,
            output_bytes,
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
            session_id: Some(session_id.to_string()),
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

    pub fn send_open_mirror_accepted(
        &self,
        session_id: &str,
        target_id: &str,
        availability: &'static str,
    ) -> Result<(), RemoteAuthorityTransportError> {
        self.send_payload(
            session_id,
            target_id,
            ControlPlanePayload::OpenMirrorAccepted(OpenMirrorAcceptedPayload {
                session_id: session_id.to_string(),
                target_id: target_id.to_string(),
                availability,
            }),
        )
    }

    pub fn send_open_mirror_rejected(
        &self,
        session_id: &str,
        target_id: &str,
        code: &'static str,
        message: impl Into<String>,
    ) -> Result<(), RemoteAuthorityTransportError> {
        self.send_payload(
            session_id,
            target_id,
            ControlPlanePayload::OpenMirrorRejected(OpenMirrorRejectedPayload {
                session_id: session_id.to_string(),
                target_id: target_id.to_string(),
                code,
                message: message.into(),
            }),
        )
    }

    pub fn send_mirror_bootstrap_chunk(
        &self,
        session_id: &str,
        target_id: &str,
        chunk_seq: u64,
        stream: &'static str,
        output_bytes: Vec<u8>,
    ) -> Result<(), RemoteAuthorityTransportError> {
        self.send_payload(
            session_id,
            target_id,
            ControlPlanePayload::MirrorBootstrapChunk(MirrorBootstrapChunkPayload {
                session_id: session_id.to_string(),
                target_id: target_id.to_string(),
                chunk_seq,
                stream,
                output_bytes,
            }),
        )
    }

    pub fn send_mirror_bootstrap_complete(
        &self,
        session_id: &str,
        target_id: &str,
        last_chunk_seq: u64,
        alternate_screen_active: bool,
        application_cursor_keys: bool,
        cursor_visible: bool,
    ) -> Result<(), RemoteAuthorityTransportError> {
        self.send_payload(
            session_id,
            target_id,
            ControlPlanePayload::MirrorBootstrapComplete(MirrorBootstrapCompletePayload {
                session_id: session_id.to_string(),
                target_id: target_id.to_string(),
                last_chunk_seq,
                alternate_screen_active,
                application_cursor_keys,
                cursor_visible,
            }),
        )
    }

    fn send_payload(
        &self,
        session_id: &str,
        target_id: &str,
        payload: ControlPlanePayload,
    ) -> Result<(), RemoteAuthorityTransportError> {
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
            session_id: Some(session_id.to_string()),
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
    let scope_hash = stable_socket_hash(&[socket_name, session_name]);
    let authority_hash = stable_socket_hash(&[target_authority_id(target)]);
    let target_hash = target_session_component(target);
    std::env::temp_dir().join(format!(
        "waitagent-remote-{scope_hash}-{authority_hash}-{target_hash}.sock",
    ))
}

pub(crate) fn authority_target_component(authority_id: &str, session_id: &str) -> String {
    stable_socket_hash(&[authority_id, ":", session_id])
}

fn target_authority_id(target: &str) -> &str {
    split_target_identity(target)
        .map(|(authority_id, _)| authority_id)
        .unwrap_or(target)
}

fn target_session_component(target: &str) -> String {
    split_target_identity(target)
        .map(|(authority_id, session_id)| authority_target_component(authority_id, session_id))
        .unwrap_or_else(|| stable_socket_hash(&[target]))
}

fn split_target_identity(target: &str) -> Option<(&str, &str)> {
    let mut parts = target.splitn(3, ':');
    let first = parts.next()?;
    let second = parts.next()?;
    let third = parts.next();

    match third {
        Some(session_id)
            if first == "remote-peer" || first == "local-tmux" || first == "remote" =>
        {
            Some((second, session_id))
        }
        Some(session_id) => Some((second, session_id)),
        None => Some((first, second)),
    }
}

fn stable_socket_hash(values: &[&str]) -> String {
    let mut hash = 0xcbf29ce484222325_u64;
    for value in values {
        for byte in value.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
    }
    format!("{hash:016x}")
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
    transport_stream.set_read_timeout(Some(AUTHORITY_TRANSPORT_READ_TIMEOUT))?;
    let node_id = read_client_hello(&mut transport_stream)?;

    let (mut local_reader, pane_stream) = UnixStream::pair()?;
    sink.submit(pane_stream).map_err(|_| {
        RemoteAuthorityTransportError::new("authority stream consumer is unavailable")
    })?;
    write_registration_frame(&mut local_reader, &node_id)?;

    let mut transport_writer = transport_stream.try_clone()?;
    write_server_hello(&mut transport_writer, AUTHORITY_TRANSPORT_SERVER_ID)?;
    let local_writer = local_reader.try_clone()?;
    // Give local_reader a short timeout so we can check the shutdown flag
    local_reader
        .set_read_timeout(Some(Duration::from_secs(5)))
        .ok();
    let running = Arc::new(AtomicBool::new(true));
    let net_running = running.clone();
    let forward_network = thread::spawn(move || {
        let result = forward_authority_frames_to_control_plane(transport_stream, local_writer);
        net_running.store(false, Ordering::Relaxed);
        result
    });
    let forward_local =
        forward_control_plane_to_authority_frames(local_reader, transport_writer, &running);
    running.store(false, Ordering::Relaxed);
    let _ = forward_network.join();
    forward_local
}

fn forward_authority_frames_to_control_plane(
    mut reader: UnixStream,
    mut writer: UnixStream,
) -> Result<(), RemoteAuthorityTransportError> {
    // Set a short read timeout so we can handle Ping/Pong and detect
    // peer death within AUTHORITY_TRANSPORT_READ_TIMEOUT seconds.
    reader
        .set_read_timeout(Some(AUTHORITY_TRANSPORT_READ_TIMEOUT))
        .ok();
    let mut last_received = Instant::now();

    loop {
        match read_authority_transport_frame(&mut reader) {
            Ok(AuthorityTransportFrame::Ping) => {
                last_received = Instant::now();
                let mut buf = Vec::new();
                let _ = write_authority_transport_frame(&mut buf, &AuthorityTransportFrame::Pong);
                let _ = reader.write_all(&buf);
            }
            Ok(AuthorityTransportFrame::Pong) => {
                last_received = Instant::now();
            }
            Ok(frame) => {
                last_received = Instant::now();
                let frame = match frame {
                    AuthorityTransportFrame::ControlPlane(envelope) => match envelope.payload {
                        ControlPlanePayload::RawPtyOutput(payload) => {
                            AuthorityTransportFrame::RawPtyOutput(payload)
                        }
                        payload => AuthorityTransportFrame::ControlPlane(ProtocolEnvelope {
                            payload,
                            ..envelope
                        }),
                    },
                    raw_frame => raw_frame,
                };
                write_authority_transport_frame(&mut writer, &frame)?;
            }
            Err(ref e) if e.is_timed_out() => {
                if last_received.elapsed() > AUTHORITY_TRANSPORT_READ_TIMEOUT {
                    return Ok(());
                }
                // Probe liveness while the remote is idle.
                let mut buf = Vec::new();
                let _ = write_authority_transport_frame(&mut buf, &AuthorityTransportFrame::Ping);
                let _ = reader.write_all(&buf);
            }
            Err(_) => return Ok(()),
        }
    }
}

fn forward_control_plane_to_authority_frames(
    mut reader: UnixStream,
    mut writer: UnixStream,
    running: &AtomicBool,
) -> Result<(), RemoteAuthorityTransportError> {
    while running.load(Ordering::Relaxed) {
        match read_authority_transport_frame(&mut reader) {
            Ok(AuthorityTransportFrame::Ping) => {
                let mut buf = Vec::new();
                let _ = write_authority_transport_frame(&mut buf, &AuthorityTransportFrame::Pong);
                let _ = reader.write_all(&buf);
            }
            Ok(AuthorityTransportFrame::Pong) => {
                // Silently consume.
            }
            Ok(frame) => match frame {
                AuthorityTransportFrame::ControlPlane(envelope) => {
                    write_control_plane_envelope(&mut writer, &envelope)?;
                }
                AuthorityTransportFrame::RawPtyInput(payload) => {
                    write_control_plane_envelope(&mut writer, &raw_pty_input_envelope(payload))?;
                }
                AuthorityTransportFrame::RawPtyOutput(payload) => {
                    write_control_plane_envelope(&mut writer, &raw_pty_output_envelope(payload))?;
                }
                // Sync frames pass through as-is to the external transport.
                AuthorityTransportFrame::SyncRequest { .. }
                | AuthorityTransportFrame::SyncResponse { .. } => {
                    write_authority_transport_frame(&mut writer, &frame)?;
                }
                _ => {}
            },
            Err(_) => {
                if !running.load(Ordering::Relaxed) {
                    break;
                }
            }
        }
    }
    Ok(())
}

fn raw_pty_input_envelope(payload: RawPtyInputPayload) -> ProtocolEnvelope<ControlPlanePayload> {
    ProtocolEnvelope {
        protocol_version: REMOTE_PROTOCOL_VERSION.to_string(),
        message_id: format!("raw-pty-input-{}", payload.input_seq),
        message_type: "raw_pty_input",
        timestamp: now_rfc3339_like(),
        sender_id: AUTHORITY_TRANSPORT_SERVER_ID.to_string(),
        correlation_id: None,
        session_id: Some(payload.session_id.clone()),
        target_id: Some(payload.target_id.clone()),
        attachment_id: Some(payload.attachment_id.clone()),
        console_id: Some(payload.console_id.clone()),
        payload: ControlPlanePayload::RawPtyInput(payload),
    }
}

fn raw_pty_output_envelope(payload: RawPtyOutputPayload) -> ProtocolEnvelope<ControlPlanePayload> {
    ProtocolEnvelope {
        protocol_version: REMOTE_PROTOCOL_VERSION.to_string(),
        message_id: format!("raw-pty-output-{}", payload.output_seq),
        message_type: "raw_pty_output",
        timestamp: now_rfc3339_like(),
        sender_id: AUTHORITY_TRANSPORT_SERVER_ID.to_string(),
        correlation_id: None,
        session_id: Some(payload.session_id.clone()),
        target_id: Some(payload.target_id.clone()),
        attachment_id: None,
        console_id: None,
        payload: ControlPlanePayload::RawPtyOutput(payload),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        authority_transport_socket_path, spawn_authority_transport_listener,
        RemoteAuthorityCommand, RemoteAuthorityTransportRuntime,
    };
    use crate::infra::remote_protocol::{
        ClientHelloPayload, ControlPlanePayload, ProtocolEnvelope, RawPtyInputPayload,
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

        assert!(rendered.contains("waitagent-remote-"));
        assert!(rendered.ends_with(".sock"));
        assert!(rendered.len() < 108);
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
                    envelope: raw_pty_input_envelope(),
                },
            ])
            .expect("raw PTY input should route to bridged authority transport");
        assert_eq!(
            transport.recv_command().expect("raw input should decode"),
            RemoteAuthorityCommand::RawPtyInput(RawPtyInputPayload {
                attachment_id: "attach-1".to_string(),
                session_id: "shell-1".to_string(),
                target_id: "remote-peer:peer-a:shell-1".to_string(),
                console_id: "console-a".to_string(),
                console_host_id: "observer-a".to_string(),
                input_seq: 8,
                input_bytes: b"\x1b[A".to_vec(),
            })
        );

        let raw_connection = registry
            .connection_for("peer-a")
            .expect("raw authority connection should be registered");
        raw_connection
            .send_raw_pty_input(&RawPtyInputPayload {
                attachment_id: "attach-1".to_string(),
                session_id: "shell-1".to_string(),
                target_id: "remote-peer:peer-a:shell-1".to_string(),
                console_id: "console-a".to_string(),
                console_host_id: "observer-a".to_string(),
                input_seq: 9,
                input_bytes: b"raw-frame".to_vec(),
            })
            .expect("raw frame input should route to bridged authority transport");
        assert_eq!(
            transport
                .recv_command()
                .expect("raw frame input should decode"),
            RemoteAuthorityCommand::RawPtyInput(RawPtyInputPayload {
                attachment_id: "attach-1".to_string(),
                session_id: "shell-1".to_string(),
                target_id: "remote-peer:peer-a:shell-1".to_string(),
                console_id: "console-a".to_string(),
                console_host_id: "observer-a".to_string(),
                input_seq: 9,
                input_bytes: b"raw-frame".to_vec(),
            })
        );

        transport
            .send_target_output(
                "shell-1",
                "remote-peer:peer-a:shell-1",
                11,
                "pty",
                b"b".to_vec(),
            )
            .expect("target output should send");
        match rx
            .recv_timeout(Duration::from_secs(1))
            .expect("output envelope should arrive")
        {
            AuthorityTransportEvent::Envelope(envelope) => match envelope.payload {
                ControlPlanePayload::TargetOutput(payload) => {
                    assert_eq!(payload.output_seq, 11);
                    assert_eq!(payload.output_bytes, b"b");
                }
                other => panic!("unexpected payload: {other:?}"),
            },
            other => panic!("unexpected event: {other:?}"),
        }
        transport
            .send_raw_pty_output("shell-1", "remote-peer:peer-a:shell-1", 12, b"c".to_vec())
            .expect("raw output should send");
        match rx
            .recv_timeout(Duration::from_secs(1))
            .expect("raw output event should arrive")
        {
            AuthorityTransportEvent::RawPtyOutput {
                authority_id,
                payload,
            } => {
                assert_eq!(authority_id, "peer-a");
                assert_eq!(payload.output_seq, 12);
                assert_eq!(payload.output_bytes, b"c");
            }
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

    fn raw_pty_input_envelope() -> ProtocolEnvelope<ControlPlanePayload> {
        ProtocolEnvelope {
            protocol_version: "1.1".to_string(),
            message_id: "msg-raw-pty-input".to_string(),
            message_type: "raw_pty_input",
            timestamp: "2026-04-28T00:00:00Z".to_string(),
            sender_id: "server".to_string(),
            correlation_id: None,
            session_id: Some("shell-1".to_string()),
            target_id: Some("remote-peer:peer-a:shell-1".to_string()),
            attachment_id: Some("attach-1".to_string()),
            console_id: Some("console-a".to_string()),
            payload: ControlPlanePayload::RawPtyInput(RawPtyInputPayload {
                attachment_id: "attach-1".to_string(),
                session_id: "shell-1".to_string(),
                target_id: "remote-peer:peer-a:shell-1".to_string(),
                console_id: "console-a".to_string(),
                console_host_id: "observer-a".to_string(),
                input_seq: 8,
                input_bytes: b"\x1b[A".to_vec(),
            }),
        }
    }
}
