use crate::infra::error_log::ERROR_LOG;
use crate::infra::remote_protocol::{
    ControlPlanePayload, ProtocolEnvelope, RawPtyInputPayload, RawPtyOutputPayload,
};
use crate::infra::remote_transport_codec::{
    read_authority_transport_frame, read_registration_frame, write_authority_transport_frame,
    write_control_plane_envelope, AuthorityTransportFrame, RemoteTransportCodecError,
};
use crate::runtime::remote_main_slot_runtime::RemoteControlPlaneTransportError;
use crate::runtime::remote_transport_runtime::{
    RemoteConnectionRegistry, RemoteControlPlaneConnection,
};
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const QUEUED_AUTHORITY_STREAM_POLL_INTERVAL: Duration = Duration::from_millis(50);
const AUTHORITY_TRANSPORT_READ_TIMEOUT: Duration = Duration::from_secs(120);
const AUTHORITY_TRANSPORT_SOCKET_TIMEOUT: Duration = Duration::from_secs(10);
const AUTHORITY_TRANSPORT_WRITE_TIMEOUT: Duration = Duration::from_millis(500);
const AUTHORITY_TRANSPORT_WRITE_RETRIES: usize = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthorityTransportEvent {
    Connected,
    Disconnected,
    Failed(String),
    Envelope(ProtocolEnvelope<ControlPlanePayload>),
    RawPtyOutput {
        authority_id: String,
        payload: RawPtyOutputPayload,
    },
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
    #[cfg(test)]
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
    let t_register = std::time::Instant::now();
    let node_id = read_registration_frame(&mut stream)?;
    if node_id != authority_id {
        return Err(RemoteAuthorityConnectionError::new(format!(
            "unexpected authority node `{node_id}`; expected `{authority_id}`"
        )));
    }

    stream.set_read_timeout(Some(AUTHORITY_TRANSPORT_SOCKET_TIMEOUT))?;
    let writer = stream.try_clone()?;
    writer.set_write_timeout(Some(AUTHORITY_TRANSPORT_WRITE_TIMEOUT))?;
    let connected = Arc::new(AtomicBool::new(true));
    let reader_tx = tx.clone();
    registry.register_connection(
        node_id.clone(),
        Arc::new(SocketRemoteControlPlaneConnection {
            writer: Arc::new(Mutex::new(writer)),
            connected: connected.clone(),
        }),
    );
    ERROR_LOG.log(format!(
        "[diag-timing] register_authority_stream: registered node={}, sending Connected ({:?})",
        node_id,
        t_register.elapsed()
    ));

    // Clone a stream handle for sending Ping/Pong frames from the reader loop.
    // The reader loop needs its own write handle because `stream` is borrowed
    // by the blocking read_authority_transport_frame call.
    let mut pong_writer = stream.try_clone()?;

    thread::spawn(move || {
        let mut last_received = Instant::now();
        let mut next_expected_output_seq: u64 = 1;
        while connected.load(Ordering::Relaxed) {
            match read_authority_transport_frame(&mut stream) {
                Ok(AuthorityTransportFrame::Ping) => {
                    last_received = Instant::now();
                    // Respond with Pong to confirm liveness.
                    let mut buf = Vec::new();
                    if write_authority_transport_frame(&mut buf, &AuthorityTransportFrame::Pong)
                        .is_ok()
                    {
                        let _ = pong_writer.write_all(&buf);
                    }
                }
                Ok(AuthorityTransportFrame::Pong) => {
                    // Remote side is alive; reset idle timer.
                    last_received = Instant::now();
                }
                Ok(AuthorityTransportFrame::ControlPlane(envelope)) => {
                    last_received = Instant::now();
                    if reader_tx
                        .send(AuthorityTransportEvent::Envelope(envelope))
                        .is_err()
                    {
                        break;
                    }
                }
                Ok(AuthorityTransportFrame::RawPtyOutput(payload)) => {
                    last_received = Instant::now();
                    // Detect gaps in the output sequence. When frames are
                    // dropped by the network or channel, the sequence number
                    // jumps. We track the expected next seq and log a gap
                    // so the operator can diagnose transport issues.
                    if payload.output_seq > 0
                        && payload.output_seq != next_expected_output_seq
                        && next_expected_output_seq > 1
                    {
                        let _ = reader_tx.send(AuthorityTransportEvent::Failed(format!(
                            "output seq gap: expected {}, got {}",
                            next_expected_output_seq, payload.output_seq
                        )));
                    }
                    next_expected_output_seq = payload.output_seq.saturating_add(1);
                    if reader_tx
                        .send(AuthorityTransportEvent::RawPtyOutput {
                            authority_id: node_id.clone(),
                            payload,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                Ok(AuthorityTransportFrame::RawPtyInput(_)) => break,
                Ok(AuthorityTransportFrame::SyncRequest { .. }) => {
                    // Remote peer is requesting replay from our output cache.
                    // The cache is managed by the authority target host runtime,
                    // not this connection loop. For now, consume silently.
                    // Full SyncRequest handling requires plumbing to the
                    // output cache (layer 1 ring buffer), which is deferred.
                    last_received = Instant::now();
                }
                Ok(AuthorityTransportFrame::SyncResponse {
                    session_id,
                    target_id,
                    seq: _,
                    bytes,
                }) => {
                    // Remote peer is replaying frames we requested.
                    // Deliver as RawPtyOutput so the display path processes them.
                    last_received = Instant::now();
                    if reader_tx
                        .send(AuthorityTransportEvent::RawPtyOutput {
                            authority_id: node_id.clone(),
                            payload: RawPtyOutputPayload {
                                session_id,
                                target_id,
                                output_seq: 0, // replayed; no sequence tracking
                                output_bytes: bytes,
                            },
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                Err(ref e) if e.is_timed_out() => {
                    // Read timed out — check total idle duration. If the remote
                    // has been silent past the timeout, consider it dead.
                    if last_received.elapsed() > AUTHORITY_TRANSPORT_READ_TIMEOUT {
                        break;
                    }
                    // Probe liveness: send Ping to check if remote is still there.
                    let mut buf = Vec::new();
                    if write_authority_transport_frame(&mut buf, &AuthorityTransportFrame::Ping)
                        .is_ok()
                    {
                        let _ = pong_writer.write_all(&buf);
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
        let mut encoded = Vec::new();
        if let Err(error) = write_control_plane_envelope(&mut encoded, envelope) {
            return Err(RemoteControlPlaneTransportError::new(error.to_string()));
        }
        write_transport_bytes_with_retries(&mut writer, &encoded).map_err(|error| {
            self.connected.store(false, Ordering::Relaxed);
            RemoteControlPlaneTransportError::new(error.to_string())
        })
    }

    fn send_raw_pty_input(
        &self,
        payload: &RawPtyInputPayload,
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
        let mut encoded = Vec::new();
        if let Err(error) = write_authority_transport_frame(
            &mut encoded,
            &AuthorityTransportFrame::RawPtyInput(payload.clone()),
        ) {
            return Err(RemoteControlPlaneTransportError::new(error.to_string()));
        }
        write_transport_bytes_with_retries(&mut writer, &encoded).map_err(|error| {
            self.connected.store(false, Ordering::Relaxed);
            RemoteControlPlaneTransportError::new(error.to_string())
        })
    }
}

fn write_transport_bytes_with_retries(writer: &mut UnixStream, bytes: &[u8]) -> io::Result<()> {
    let mut written = 0usize;
    let mut retries = 0usize;
    while written < bytes.len() {
        match writer.write(&bytes[written..]) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "authority transport write returned zero bytes",
                ));
            }
            Ok(count) => {
                written += count;
                retries = 0;
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::Interrupted
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::WouldBlock
                ) && retries < AUTHORITY_TRANSPORT_WRITE_RETRIES =>
            {
                retries += 1;
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
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
mod remote_authority_connection_runtime_test;
