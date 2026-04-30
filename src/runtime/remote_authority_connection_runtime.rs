use crate::infra::remote_protocol::{ControlPlanePayload, ProtocolEnvelope};
use crate::infra::remote_transport_codec::{
    read_control_plane_envelope, read_registration_frame, write_control_plane_envelope,
    RemoteTransportCodecError,
};
use crate::runtime::remote_main_slot_runtime::RemoteControlPlaneTransportError;
use crate::runtime::remote_transport_runtime::{
    RemoteConnectionRegistry, RemoteControlPlaneConnection,
};
use std::fmt;
use std::fs;
use std::io;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const QUEUED_AUTHORITY_STREAM_POLL_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthorityTransportEvent {
    Connected,
    Disconnected,
    Failed(String),
    Envelope(ProtocolEnvelope<ControlPlanePayload>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorityConnectionRequest {
    pub socket_path: PathBuf,
    pub authority_id: String,
}

pub struct AuthorityConnectionListenerGuard {
    socket_path: PathBuf,
}

struct SocketRemoteControlPlaneConnection {
    writer: Arc<Mutex<UnixStream>>,
    connected: Arc<AtomicBool>,
}

pub trait AuthorityConnectionSource {
    type Guard;

    fn start(
        &self,
        request: AuthorityConnectionRequest,
        registry: RemoteConnectionRegistry,
        tx: mpsc::Sender<AuthorityTransportEvent>,
    ) -> io::Result<Self::Guard>;
}

pub trait AuthorityConnectionGuard: Send {}

impl<T> AuthorityConnectionGuard for T where T: Send {}

pub trait AuthorityConnectionStarter: Send + Sync {
    fn start_connection(
        &self,
        request: AuthorityConnectionRequest,
        registry: RemoteConnectionRegistry,
        tx: mpsc::Sender<AuthorityTransportEvent>,
    ) -> io::Result<Box<dyn AuthorityConnectionGuard>>;
}

#[derive(Clone, Default)]
pub struct LocalAuthoritySocketSource;

pub struct QueuedAuthorityStreamSource {
    receiver: Mutex<Option<mpsc::Receiver<UnixStream>>>,
}

#[derive(Clone)]
pub struct QueuedAuthorityStreamSink {
    sender: mpsc::Sender<UnixStream>,
}

#[derive(Clone, Default)]
pub struct LocalAuthoritySocketBridgeStarter;

pub struct QueuedAuthorityStreamStarter {
    runtime: RemoteAuthorityConnectionRuntime<QueuedAuthorityStreamSource>,
}

pub struct RemoteAuthorityConnectionRuntime<S = LocalAuthoritySocketSource> {
    source: S,
}

impl<S> RemoteAuthorityConnectionRuntime<S>
where
    S: AuthorityConnectionSource,
{
    pub fn new(source: S) -> Self {
        Self { source }
    }

    pub fn start_connection_source(
        &self,
        request: AuthorityConnectionRequest,
        registry: RemoteConnectionRegistry,
        tx: mpsc::Sender<AuthorityTransportEvent>,
    ) -> io::Result<S::Guard> {
        self.source.start(request, registry, tx)
    }
}

impl RemoteAuthorityConnectionRuntime<LocalAuthoritySocketSource> {
    pub fn with_local_socket_source() -> Self {
        Self::new(LocalAuthoritySocketSource)
    }
}

impl<S> AuthorityConnectionStarter for RemoteAuthorityConnectionRuntime<S>
where
    S: AuthorityConnectionSource + Send + Sync,
    S::Guard: AuthorityConnectionGuard + 'static,
{
    fn start_connection(
        &self,
        request: AuthorityConnectionRequest,
        registry: RemoteConnectionRegistry,
        tx: mpsc::Sender<AuthorityTransportEvent>,
    ) -> io::Result<Box<dyn AuthorityConnectionGuard>> {
        Ok(Box::new(
            self.start_connection_source(request, registry, tx)?,
        ))
    }
}

impl QueuedAuthorityStreamSource {
    pub fn channel() -> (Self, QueuedAuthorityStreamSink) {
        let (sender, receiver) = mpsc::channel();
        (
            Self {
                receiver: Mutex::new(Some(receiver)),
            },
            QueuedAuthorityStreamSink { sender },
        )
    }
}

impl QueuedAuthorityStreamStarter {
    pub fn channel() -> (Self, QueuedAuthorityStreamSink) {
        let (source, sink) = QueuedAuthorityStreamSource::channel();
        (
            Self {
                runtime: RemoteAuthorityConnectionRuntime::new(source),
            },
            sink,
        )
    }
}

pub fn spawn_authority_listener(
    request: AuthorityConnectionRequest,
    registry: RemoteConnectionRegistry,
    tx: mpsc::Sender<AuthorityTransportEvent>,
) -> io::Result<AuthorityConnectionListenerGuard> {
    if request.socket_path.exists() {
        let _ = fs::remove_file(&request.socket_path);
    }
    let listener = UnixListener::bind(&request.socket_path)?;
    let AuthorityConnectionRequest {
        socket_path,
        authority_id,
    } = request;

    thread::spawn(move || {
        for accepted in listener.incoming() {
            let Ok(stream) = accepted else {
                break;
            };
            if let Err(error) = register_authority_stream(
                stream,
                registry.clone(),
                authority_id.clone(),
                tx.clone(),
            ) {
                let _ = tx.send(AuthorityTransportEvent::Failed(error.to_string()));
            }
        }
    });

    Ok(AuthorityConnectionListenerGuard { socket_path })
}

pub fn register_authority_stream(
    mut stream: UnixStream,
    registry: RemoteConnectionRegistry,
    authority_id: String,
    tx: mpsc::Sender<AuthorityTransportEvent>,
) -> Result<(), RemoteAuthorityConnectionError> {
    let node_id = read_registration_frame(&mut stream)?;
    if node_id != authority_id {
        return Err(RemoteAuthorityConnectionError::new(format!(
            "unexpected authority node `{node_id}`; expected `{authority_id}`"
        )));
    }

    let writer = stream.try_clone()?;
    let connected = Arc::new(AtomicBool::new(true));
    let reader_tx = tx.clone();
    registry.register_connection(
        node_id.clone(),
        Arc::new(SocketRemoteControlPlaneConnection {
            writer: Arc::new(Mutex::new(writer)),
            connected: connected.clone(),
        }),
    );

    thread::spawn(move || {
        while connected.load(Ordering::Relaxed) {
            match read_control_plane_envelope(&mut stream) {
                Ok(envelope) => {
                    if reader_tx
                        .send(AuthorityTransportEvent::Envelope(envelope))
                        .is_err()
                    {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        connected.store(false, Ordering::Relaxed);
        registry.unregister_connection(&node_id);
        let _ = reader_tx.send(AuthorityTransportEvent::Disconnected);
    });

    let _ = tx.send(AuthorityTransportEvent::Connected);
    Ok(())
}

impl Drop for AuthorityConnectionListenerGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.socket_path);
    }
}

impl AuthorityConnectionSource for LocalAuthoritySocketSource {
    type Guard = AuthorityConnectionListenerGuard;

    fn start(
        &self,
        request: AuthorityConnectionRequest,
        registry: RemoteConnectionRegistry,
        tx: mpsc::Sender<AuthorityTransportEvent>,
    ) -> io::Result<Self::Guard> {
        spawn_authority_listener(request, registry, tx)
    }
}

pub struct QueuedAuthorityStreamSourceGuard {
    running: Arc<AtomicBool>,
}

impl AuthorityConnectionSource for QueuedAuthorityStreamSource {
    type Guard = QueuedAuthorityStreamSourceGuard;

    fn start(
        &self,
        request: AuthorityConnectionRequest,
        registry: RemoteConnectionRegistry,
        tx: mpsc::Sender<AuthorityTransportEvent>,
    ) -> io::Result<Self::Guard> {
        let receiver = self
            .receiver
            .lock()
            .expect("queued authority stream source mutex should not be poisoned")
            .take()
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "queued authority stream source already started",
                )
            })?;
        let running = Arc::new(AtomicBool::new(true));
        let worker_running = running.clone();
        let authority_id = request.authority_id;
        thread::spawn(move || {
            while worker_running.load(Ordering::Relaxed) {
                match receiver.recv_timeout(QUEUED_AUTHORITY_STREAM_POLL_INTERVAL) {
                    Ok(stream) => {
                        if let Err(error) = register_authority_stream(
                            stream,
                            registry.clone(),
                            authority_id.clone(),
                            tx.clone(),
                        ) {
                            let _ = tx.send(AuthorityTransportEvent::Failed(error.to_string()));
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
        });
        Ok(QueuedAuthorityStreamSourceGuard { running })
    }
}

impl Drop for QueuedAuthorityStreamSourceGuard {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
    }
}

impl QueuedAuthorityStreamSink {
    pub fn submit(&self, stream: UnixStream) -> Result<(), mpsc::SendError<UnixStream>> {
        self.sender.send(stream)
    }
}

pub struct AuthorityStreamProducerGuard {
    socket_path: PathBuf,
}

struct LocalAuthoritySocketBridgeGuard {
    _producer: AuthorityStreamProducerGuard,
    _source: QueuedAuthorityStreamSourceGuard,
}

impl AuthorityConnectionStarter for LocalAuthoritySocketBridgeStarter {
    fn start_connection(
        &self,
        request: AuthorityConnectionRequest,
        registry: RemoteConnectionRegistry,
        tx: mpsc::Sender<AuthorityTransportEvent>,
    ) -> io::Result<Box<dyn AuthorityConnectionGuard>> {
        let socket_path = request.socket_path.clone();
        let (source, sink) = QueuedAuthorityStreamSource::channel();
        let runtime = RemoteAuthorityConnectionRuntime::new(source);
        let source_guard = runtime.start_connection_source(request, registry, tx)?;
        let producer_guard = spawn_authority_stream_producer(socket_path, sink)?;
        Ok(Box::new(LocalAuthoritySocketBridgeGuard {
            _producer: producer_guard,
            _source: source_guard,
        }))
    }
}

impl AuthorityConnectionStarter for QueuedAuthorityStreamStarter {
    fn start_connection(
        &self,
        request: AuthorityConnectionRequest,
        registry: RemoteConnectionRegistry,
        tx: mpsc::Sender<AuthorityTransportEvent>,
    ) -> io::Result<Box<dyn AuthorityConnectionGuard>> {
        Ok(Box::new(
            self.runtime
                .start_connection_source(request, registry, tx)?,
        ))
    }
}

pub fn spawn_authority_stream_producer(
    socket_path: PathBuf,
    sink: QueuedAuthorityStreamSink,
) -> io::Result<AuthorityStreamProducerGuard> {
    if socket_path.exists() {
        let _ = fs::remove_file(&socket_path);
    }
    let listener = UnixListener::bind(&socket_path)?;
    thread::spawn(move || {
        for accepted in listener.incoming() {
            let Ok(stream) = accepted else {
                break;
            };
            if sink.submit(stream).is_err() {
                break;
            }
        }
    });
    Ok(AuthorityStreamProducerGuard { socket_path })
}

impl Drop for AuthorityStreamProducerGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.socket_path);
    }
}

impl RemoteControlPlaneConnection for SocketRemoteControlPlaneConnection {
    fn send(
        &self,
        envelope: &ProtocolEnvelope<ControlPlanePayload>,
    ) -> Result<(), RemoteControlPlaneTransportError> {
        if !self.connected.load(Ordering::Relaxed) {
            return Err(RemoteControlPlaneTransportError::new(
                "authority transport connection is closed",
            ));
        }
        let mut writer = self
            .writer
            .lock()
            .expect("authority transport writer mutex should not be poisoned");
        write_control_plane_envelope(&mut *writer, envelope)
            .map_err(|error| RemoteControlPlaneTransportError::new(error.to_string()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteAuthorityConnectionError {
    message: String,
}

impl RemoteAuthorityConnectionError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for RemoteAuthorityConnectionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for RemoteAuthorityConnectionError {}

impl From<io::Error> for RemoteAuthorityConnectionError {
    fn from(value: io::Error) -> Self {
        Self::new(value.to_string())
    }
}

impl From<RemoteTransportCodecError> for RemoteAuthorityConnectionError {
    fn from(value: RemoteTransportCodecError) -> Self {
        Self::new(value.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        register_authority_stream, spawn_authority_listener, AuthorityConnectionRequest,
        AuthorityConnectionStarter, AuthorityTransportEvent, LocalAuthoritySocketBridgeStarter,
        QueuedAuthorityStreamSource, QueuedAuthorityStreamStarter,
        RemoteAuthorityConnectionRuntime,
    };
    use crate::infra::remote_protocol::{
        ControlPlanePayload, ProtocolEnvelope, TargetOutputPayload,
    };
    use crate::infra::remote_transport_codec::{
        write_control_plane_envelope, write_registration_frame,
    };
    use crate::runtime::remote_transport_runtime::RemoteConnectionRegistry;
    use std::fs;
    use std::os::unix::net::UnixStream;
    use std::process;
    use std::sync::mpsc;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    #[test]
    fn register_authority_stream_tracks_connection_and_forwards_inbound_envelopes() {
        let registry = RemoteConnectionRegistry::new();
        let (tx, rx) = mpsc::channel();
        let (mut client, server) = UnixStream::pair().expect("stream pair should open");

        write_registration_frame(&mut client, "peer-a").expect("registration frame should encode");
        register_authority_stream(server, registry.clone(), "peer-a".to_string(), tx)
            .expect("authority stream should register");

        assert!(registry.has_connection("peer-a"));
        assert_eq!(
            rx.recv().expect("transport event should be emitted"),
            AuthorityTransportEvent::Connected
        );

        write_control_plane_envelope(&mut client, &authority_target_output_envelope(1))
            .expect("target output should encode");
        match rx.recv().expect("authority envelope should arrive") {
            AuthorityTransportEvent::Envelope(envelope) => {
                assert_eq!(envelope.sender_id, "peer-a");
                match envelope.payload {
                    ControlPlanePayload::TargetOutput(payload) => {
                        assert_eq!(payload.output_seq, 1);
                        assert_eq!(payload.bytes_base64, "YQ==");
                    }
                    other => panic!("unexpected payload: {other:?}"),
                }
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn register_authority_stream_rejects_unexpected_node_id() {
        let registry = RemoteConnectionRegistry::new();
        let (tx, _rx) = mpsc::channel();
        let (mut client, server) = UnixStream::pair().expect("stream pair should open");

        write_registration_frame(&mut client, "peer-b").expect("registration frame should encode");
        let error = register_authority_stream(server, registry.clone(), "peer-a".to_string(), tx)
            .expect_err("unexpected authority node should fail");

        assert_eq!(
            error.to_string(),
            "unexpected authority node `peer-b`; expected `peer-a`"
        );
        assert!(!registry.has_connection("peer-a"));
        assert!(!registry.has_connection("peer-b"));
    }

    #[test]
    fn spawned_listener_reports_failed_registrations_without_registering_connections() {
        let registry = RemoteConnectionRegistry::new();
        let (tx, rx) = mpsc::channel();
        let socket_path = test_socket_path("failed-registration");
        let _guard = spawn_authority_listener(
            AuthorityConnectionRequest {
                socket_path: socket_path.clone(),
                authority_id: "peer-a".to_string(),
            },
            registry.clone(),
            tx,
        )
        .expect("authority listener should bind");

        let mut stream = UnixStream::connect(&socket_path).expect("listener should accept");
        write_registration_frame(&mut stream, "peer-b").expect("registration frame should encode");

        assert_eq!(
            rx.recv_timeout(Duration::from_secs(1))
                .expect("failure event should arrive"),
            AuthorityTransportEvent::Failed(
                "unexpected authority node `peer-b`; expected `peer-a`".to_string()
            )
        );
        assert!(!registry.has_connection("peer-a"));
        assert!(!registry.has_connection("peer-b"));
        let _ = fs::remove_file(&socket_path);
    }

    #[test]
    fn spawned_listener_accepts_authority_transport_connections() {
        let registry = RemoteConnectionRegistry::new();
        let (tx, rx) = mpsc::channel();
        let socket_path = test_socket_path("accept");
        let _guard = spawn_authority_listener(
            AuthorityConnectionRequest {
                socket_path: socket_path.clone(),
                authority_id: "peer-a".to_string(),
            },
            registry.clone(),
            tx,
        )
        .expect("authority listener should bind");

        let mut stream = UnixStream::connect(&socket_path).expect("listener should accept");
        write_registration_frame(&mut stream, "peer-a").expect("registration frame should encode");

        assert_eq!(
            rx.recv_timeout(Duration::from_secs(1))
                .expect("connected event should arrive"),
            AuthorityTransportEvent::Connected
        );
        assert!(registry.has_connection("peer-a"));
        let _ = fs::remove_file(&socket_path);
    }

    #[test]
    fn runtime_with_local_socket_source_starts_listener_through_source_boundary() {
        let runtime = RemoteAuthorityConnectionRuntime::with_local_socket_source();
        let registry = RemoteConnectionRegistry::new();
        let (tx, rx) = mpsc::channel();
        let socket_path = test_socket_path("runtime-local-source");
        let _guard = runtime
            .start_connection_source(
                AuthorityConnectionRequest {
                    socket_path: socket_path.clone(),
                    authority_id: "peer-a".to_string(),
                },
                registry.clone(),
                tx,
            )
            .expect("runtime should start local socket source");

        let mut stream = UnixStream::connect(&socket_path).expect("listener should accept");
        write_registration_frame(&mut stream, "peer-a").expect("registration frame should encode");

        assert_eq!(
            rx.recv_timeout(Duration::from_secs(1))
                .expect("connected event should arrive"),
            AuthorityTransportEvent::Connected
        );
        assert!(registry.has_connection("peer-a"));
        let _ = fs::remove_file(&socket_path);
    }

    #[test]
    fn queued_stream_source_accepts_injected_authority_streams() {
        let (source, sink) = QueuedAuthorityStreamSource::channel();
        let runtime = RemoteAuthorityConnectionRuntime::new(source);
        let registry = RemoteConnectionRegistry::new();
        let (tx, rx) = mpsc::channel();
        let _guard = runtime
            .start_connection_source(
                AuthorityConnectionRequest {
                    socket_path: test_socket_path("queued-unused"),
                    authority_id: "peer-a".to_string(),
                },
                registry.clone(),
                tx,
            )
            .expect("queued source should start");

        let (mut client, server) = UnixStream::pair().expect("stream pair should open");
        sink.submit(server)
            .expect("queued source should accept injected stream");
        write_registration_frame(&mut client, "peer-a").expect("registration frame should encode");

        assert_eq!(
            rx.recv_timeout(Duration::from_secs(1))
                .expect("connected event should arrive"),
            AuthorityTransportEvent::Connected
        );
        assert!(registry.has_connection("peer-a"));

        write_control_plane_envelope(&mut client, &authority_target_output_envelope(7))
            .expect("target output should encode");
        match rx
            .recv_timeout(Duration::from_secs(1))
            .expect("authority envelope should arrive")
        {
            AuthorityTransportEvent::Envelope(envelope) => match envelope.payload {
                ControlPlanePayload::TargetOutput(payload) => {
                    assert_eq!(payload.output_seq, 7);
                    assert_eq!(payload.bytes_base64, "YQ==");
                }
                other => panic!("unexpected payload: {other:?}"),
            },
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn queued_stream_source_reports_failed_registration_from_injected_stream() {
        let (source, sink) = QueuedAuthorityStreamSource::channel();
        let runtime = RemoteAuthorityConnectionRuntime::new(source);
        let registry = RemoteConnectionRegistry::new();
        let (tx, rx) = mpsc::channel();
        let _guard = runtime
            .start_connection_source(
                AuthorityConnectionRequest {
                    socket_path: test_socket_path("queued-failed-unused"),
                    authority_id: "peer-a".to_string(),
                },
                registry.clone(),
                tx,
            )
            .expect("queued source should start");

        let (mut client, server) = UnixStream::pair().expect("stream pair should open");
        sink.submit(server)
            .expect("queued source should accept injected stream");
        write_registration_frame(&mut client, "peer-b").expect("registration frame should encode");

        assert_eq!(
            rx.recv_timeout(Duration::from_secs(1))
                .expect("failed event should arrive"),
            AuthorityTransportEvent::Failed(
                "unexpected authority node `peer-b`; expected `peer-a`".to_string()
            )
        );
        assert!(!registry.has_connection("peer-a"));
        assert!(!registry.has_connection("peer-b"));
    }

    #[test]
    fn local_socket_bridge_starter_feeds_listener_streams_through_queued_source() {
        let starter = LocalAuthoritySocketBridgeStarter;
        let registry = RemoteConnectionRegistry::new();
        let (tx, rx) = mpsc::channel();
        let socket_path = test_socket_path("bridge-starter");
        let _guard = starter
            .start_connection(
                AuthorityConnectionRequest {
                    socket_path: socket_path.clone(),
                    authority_id: "peer-a".to_string(),
                },
                registry.clone(),
                tx,
            )
            .expect("bridge starter should start");

        let mut stream = UnixStream::connect(&socket_path).expect("bridge listener should accept");
        write_registration_frame(&mut stream, "peer-a").expect("registration frame should encode");

        assert_eq!(
            rx.recv_timeout(Duration::from_secs(1))
                .expect("connected event should arrive"),
            AuthorityTransportEvent::Connected
        );
        assert!(registry.has_connection("peer-a"));

        write_control_plane_envelope(&mut stream, &authority_target_output_envelope(9))
            .expect("target output should encode");
        match rx
            .recv_timeout(Duration::from_secs(1))
            .expect("authority envelope should arrive")
        {
            AuthorityTransportEvent::Envelope(envelope) => match envelope.payload {
                ControlPlanePayload::TargetOutput(payload) => {
                    assert_eq!(payload.output_seq, 9);
                    assert_eq!(payload.bytes_base64, "YQ==");
                }
                other => panic!("unexpected payload: {other:?}"),
            },
            other => panic!("unexpected event: {other:?}"),
        }
        let _ = fs::remove_file(&socket_path);
    }

    #[test]
    fn queued_stream_starter_exposes_external_producer_boundary() {
        let (starter, sink) = QueuedAuthorityStreamStarter::channel();
        let registry = RemoteConnectionRegistry::new();
        let (tx, rx) = mpsc::channel();
        let _guard = starter
            .start_connection(
                AuthorityConnectionRequest {
                    socket_path: test_socket_path("queued-starter-unused"),
                    authority_id: "peer-a".to_string(),
                },
                registry.clone(),
                tx,
            )
            .expect("queued stream starter should start");

        let (mut client, server) = UnixStream::pair().expect("stream pair should open");
        sink.submit(server)
            .expect("queued stream starter should accept injected stream");
        write_registration_frame(&mut client, "peer-a").expect("registration frame should encode");

        assert_eq!(
            rx.recv_timeout(Duration::from_secs(1))
                .expect("connected event should arrive"),
            AuthorityTransportEvent::Connected
        );
        assert!(registry.has_connection("peer-a"));

        write_control_plane_envelope(&mut client, &authority_target_output_envelope(13))
            .expect("target output should encode");
        match rx
            .recv_timeout(Duration::from_secs(1))
            .expect("authority envelope should arrive")
        {
            AuthorityTransportEvent::Envelope(envelope) => match envelope.payload {
                ControlPlanePayload::TargetOutput(payload) => {
                    assert_eq!(payload.output_seq, 13);
                    assert_eq!(payload.bytes_base64, "YQ==");
                }
                other => panic!("unexpected payload: {other:?}"),
            },
            other => panic!("unexpected event: {other:?}"),
        }
    }

    fn authority_target_output_envelope(output_seq: u64) -> ProtocolEnvelope<ControlPlanePayload> {
        ProtocolEnvelope {
            protocol_version: "1.1".to_string(),
            message_id: format!("msg-{output_seq}"),
            message_type: "target_output",
            timestamp: "2026-04-28T00:00:00Z".to_string(),
            sender_id: "peer-a".to_string(),
            correlation_id: None,
            session_id: Some("shell-1".to_string()),
            target_id: Some("remote-peer:peer-a:shell-1".to_string()),
            attachment_id: None,
            console_id: None,
            payload: ControlPlanePayload::TargetOutput(TargetOutputPayload {
                session_id: "shell-1".to_string(),
                target_id: "remote-peer:peer-a:shell-1".to_string(),
                output_seq,
                stream: "pty",
                bytes_base64: "YQ==".to_string(),
            }),
        }
    }

    fn test_socket_path(name: &str) -> std::path::PathBuf {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        std::env::temp_dir().join(format!(
            "waitagent-test-authority-connection-{name}-{}-{millis}.sock",
            process::id()
        ))
    }
}
