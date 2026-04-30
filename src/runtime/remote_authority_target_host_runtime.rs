use crate::cli::{
    RemoteAuthorityOutputPumpCommand, RemoteAuthorityTargetHostCommand, RemoteNetworkConfig,
};
use crate::infra::base64::{decode_base64, encode_base64};
use crate::infra::tmux::{EmbeddedTmuxBackend, TmuxError, TmuxPaneId};
use crate::lifecycle::LifecycleError;
use crate::runtime::remote_authority_transport_runtime::{
    RemoteAuthorityCommand, RemoteAuthorityTransportRuntime,
};
use crate::runtime::remote_node_session_owner_runtime::live_authority_session_socket_path;
use crate::runtime::remote_target_publication_runtime::{
    signal_publication_sender_live_session_registered,
    signal_publication_sender_live_session_unregistered, RemoteTargetPublicationRuntime,
};
use std::fmt;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;

pub trait RemoteTargetPtyGateway: Send + Sync + Clone + 'static {
    type Error: ToString;

    fn target_main_pane(
        &self,
        socket_name: &str,
        target_session_name: &str,
    ) -> Result<TmuxPaneId, Self::Error>;

    fn send_input(
        &self,
        socket_name: &str,
        pane: &TmuxPaneId,
        bytes: &[u8],
    ) -> Result<(), Self::Error>;

    fn resize_pty(
        &self,
        socket_name: &str,
        pane: &TmuxPaneId,
        cols: usize,
        rows: usize,
    ) -> Result<(), Self::Error>;

    fn clear_output_pipe(&self, socket_name: &str, pane: &TmuxPaneId) -> Result<(), Self::Error>;

    fn set_output_pipe(
        &self,
        socket_name: &str,
        pane: &TmuxPaneId,
        command: &str,
    ) -> Result<(), Self::Error>;
}

impl RemoteTargetPtyGateway for EmbeddedTmuxBackend {
    type Error = TmuxError;

    fn target_main_pane(
        &self,
        socket_name: &str,
        target_session_name: &str,
    ) -> Result<TmuxPaneId, Self::Error> {
        self.target_main_pane_on_socket(socket_name, target_session_name)
    }

    fn send_input(
        &self,
        socket_name: &str,
        pane: &TmuxPaneId,
        bytes: &[u8],
    ) -> Result<(), Self::Error> {
        self.send_input_to_pane_on_socket(socket_name, pane, bytes)
    }

    fn resize_pty(
        &self,
        socket_name: &str,
        pane: &TmuxPaneId,
        cols: usize,
        rows: usize,
    ) -> Result<(), Self::Error> {
        self.resize_pane_on_socket(socket_name, pane, cols, rows)
    }

    fn clear_output_pipe(&self, socket_name: &str, pane: &TmuxPaneId) -> Result<(), Self::Error> {
        self.clear_pane_pipe_on_socket(socket_name, pane)
    }

    fn set_output_pipe(
        &self,
        socket_name: &str,
        pane: &TmuxPaneId,
        command: &str,
    ) -> Result<(), Self::Error> {
        self.set_pane_pipe_on_socket(socket_name, pane, command)
    }
}

pub trait RemoteAuthorityPublicationGateway: Send + Sync + Clone + 'static {
    fn ensure_live_session_registered(
        &self,
        socket_name: &str,
        target_session_name: &str,
        authority_id: &str,
        target_id: &str,
        transport_socket_path: &str,
    ) -> Result<PathBuf, LifecycleError>;

    fn ensure_live_session_unregistered(
        &self,
        socket_name: &str,
        target_session_name: &str,
    ) -> Result<(), LifecycleError>;

    fn start_source_publication(
        &self,
        socket_name: &str,
        target_session_name: &str,
    ) -> Result<(), LifecycleError>;

    fn stop_source_publication(
        &self,
        socket_name: &str,
        target_session_name: &str,
    ) -> Result<(), LifecycleError>;
}

impl RemoteAuthorityPublicationGateway for RemoteTargetPublicationRuntime {
    fn ensure_live_session_registered(
        &self,
        socket_name: &str,
        target_session_name: &str,
        authority_id: &str,
        target_id: &str,
        transport_socket_path: &str,
    ) -> Result<PathBuf, LifecycleError> {
        self.ensure_publication_sender_running(socket_name)?;
        signal_publication_sender_live_session_registered(
            socket_name,
            target_session_name,
            authority_id,
            target_id,
            transport_socket_path,
        )?;
        let authority_socket_path =
            live_authority_session_socket_path(socket_name, target_session_name);
        wait_for_ready_socket(&authority_socket_path)?;
        Ok(authority_socket_path)
    }

    fn ensure_live_session_unregistered(
        &self,
        socket_name: &str,
        target_session_name: &str,
    ) -> Result<(), LifecycleError> {
        signal_publication_sender_live_session_unregistered(socket_name, target_session_name)
    }

    fn start_source_publication(
        &self,
        socket_name: &str,
        target_session_name: &str,
    ) -> Result<(), LifecycleError> {
        self.signal_source_session_refresh(socket_name, target_session_name)
    }

    fn stop_source_publication(
        &self,
        socket_name: &str,
        target_session_name: &str,
    ) -> Result<(), LifecycleError> {
        self.signal_source_session_closed(socket_name, target_session_name)?;
        Ok(())
    }
}

pub struct RemoteAuthorityTargetHostRuntime<
    G = EmbeddedTmuxBackend,
    P = RemoteTargetPublicationRuntime,
> {
    gateway: G,
    publication_gateway: P,
    current_executable: PathBuf,
}

enum AuthorityHostEvent {
    TransportCommand(RemoteAuthorityCommand),
    OutputChunk(Vec<u8>),
    TransportClosed,
}

struct OutputPipeGuard<G>
where
    G: RemoteTargetPtyGateway,
{
    gateway: G,
    socket_name: String,
    pane: TmuxPaneId,
    ingest_socket_path: PathBuf,
}

pub struct RemoteAuthorityOutputPumpRuntime;

impl RemoteAuthorityTargetHostRuntime<EmbeddedTmuxBackend, RemoteTargetPublicationRuntime> {
    pub fn from_build_env(network: RemoteNetworkConfig) -> Result<Self, LifecycleError> {
        let gateway = EmbeddedTmuxBackend::from_build_env().map_err(remote_authority_error)?;
        let publication_gateway =
            RemoteTargetPublicationRuntime::from_build_env_with_network(network)?;
        let current_executable = std::env::current_exe().map_err(|error| {
            LifecycleError::Io(
                "failed to locate current waitagent executable".to_string(),
                error,
            )
        })?;
        Ok(Self::new(gateway, publication_gateway, current_executable))
    }
}

impl<G, P> RemoteAuthorityTargetHostRuntime<G, P>
where
    G: RemoteTargetPtyGateway,
    P: RemoteAuthorityPublicationGateway,
{
    pub fn new(gateway: G, publication_gateway: P, current_executable: PathBuf) -> Self {
        Self {
            gateway,
            publication_gateway,
            current_executable,
        }
    }

    pub fn run_target_host(
        &self,
        command: RemoteAuthorityTargetHostCommand,
    ) -> Result<(), LifecycleError> {
        let pane = self
            .gateway
            .target_main_pane(&command.socket_name, &command.target_session_name)
            .map_err(remote_authority_error)?;
        let authority_socket_path = self
            .publication_gateway
            .ensure_live_session_registered(
                &command.socket_name,
                &command.target_session_name,
                &command.authority_id,
                &command.target_id,
                &command.transport_socket_path,
            )
            .map_err(remote_authority_error)?;
        let transport = Arc::new(
            RemoteAuthorityTransportRuntime::connect(&authority_socket_path, &command.authority_id)
                .map_err(remote_authority_error)?,
        );
        let ingest_socket_path =
            authority_output_ingest_socket_path(&command.transport_socket_path, &command.target_id);
        let listener =
            bind_output_ingest_listener(&ingest_socket_path).map_err(remote_authority_error)?;
        let pipe_command = remote_authority_output_pump_shell_command(
            self.current_executable.to_string_lossy().as_ref(),
            &ingest_socket_path,
        );
        self.gateway
            .clear_output_pipe(&command.socket_name, &pane)
            .map_err(remote_authority_error)?;
        self.gateway
            .set_output_pipe(&command.socket_name, &pane, &pipe_command)
            .map_err(remote_authority_error)?;
        let _output_guard = OutputPipeGuard {
            gateway: self.gateway.clone(),
            socket_name: command.socket_name.clone(),
            pane: pane.clone(),
            ingest_socket_path: ingest_socket_path.clone(),
        };

        let (event_tx, event_rx) = mpsc::channel();
        let _ = self
            .publication_gateway
            .start_source_publication(&command.socket_name, &command.target_session_name);
        let reader_transport = transport.clone();
        let reader_tx = event_tx.clone();
        let command_thread = thread::spawn(move || {
            while let Ok(command) = reader_transport.recv_command() {
                if reader_tx
                    .send(AuthorityHostEvent::TransportCommand(command))
                    .is_err()
                {
                    return;
                }
            }
            let _ = reader_tx.send(AuthorityHostEvent::TransportClosed);
        });

        let running = Arc::new(AtomicBool::new(true));
        let output_thread = spawn_output_ingest_thread(listener, running.clone(), event_tx);
        let mut output_seq = 0_u64;

        let loop_result = loop {
            match event_rx.recv() {
                Ok(AuthorityHostEvent::TransportCommand(RemoteAuthorityCommand::TargetInput(
                    payload,
                ))) => {
                    let bytes = match decode_base64(&payload.bytes_base64)
                        .map_err(remote_authority_error)
                    {
                        Ok(bytes) => bytes,
                        Err(error) => break Err(error),
                    };
                    if let Err(error) = self
                        .gateway
                        .send_input(&command.socket_name, &pane, &bytes)
                        .map_err(remote_authority_error)
                    {
                        break Err(error);
                    }
                }
                Ok(AuthorityHostEvent::TransportCommand(RemoteAuthorityCommand::ApplyResize(
                    payload,
                ))) => {
                    if let Err(error) = self
                        .gateway
                        .resize_pty(&command.socket_name, &pane, payload.cols, payload.rows)
                        .map_err(remote_authority_error)
                    {
                        break Err(error);
                    }
                }
                Ok(AuthorityHostEvent::OutputChunk(bytes)) => {
                    output_seq += 1;
                    if let Err(error) = transport
                        .send_target_output(
                            &command.target_id,
                            output_seq,
                            "pty",
                            encode_base64(&bytes),
                        )
                        .map_err(remote_authority_error)
                    {
                        break Err(error);
                    }
                }
                Ok(AuthorityHostEvent::TransportClosed) => {
                    if let Err(error) = self
                        .publication_gateway
                        .stop_source_publication(&command.socket_name, &command.target_session_name)
                        .map_err(remote_authority_error)
                    {
                        break Err(error);
                    }
                    break Ok(());
                }
                Err(_) => break Ok(()),
            }
        };

        running.store(false, Ordering::Relaxed);
        let _ = UnixStream::connect(&ingest_socket_path);
        let _ = self
            .publication_gateway
            .ensure_live_session_unregistered(&command.socket_name, &command.target_session_name);
        let _ = command_thread.join();
        let _ = output_thread.join();
        loop_result
    }

    pub fn run_output_pump(
        &self,
        command: RemoteAuthorityOutputPumpCommand,
    ) -> Result<(), LifecycleError> {
        RemoteAuthorityOutputPumpRuntime::run(command)
    }
}

impl RemoteAuthorityOutputPumpRuntime {
    pub fn run(command: RemoteAuthorityOutputPumpCommand) -> Result<(), LifecycleError> {
        let stdin = io::stdin();
        pump_reader_to_ingest_socket(stdin.lock(), &command.ingest_socket_path)
            .map_err(remote_authority_error)
    }
}

impl<G> Drop for OutputPipeGuard<G>
where
    G: RemoteTargetPtyGateway,
{
    fn drop(&mut self) {
        let _ = self
            .gateway
            .clear_output_pipe(&self.socket_name, &self.pane);
        let _ = fs::remove_file(&self.ingest_socket_path);
    }
}

fn bind_output_ingest_listener(
    socket_path: &Path,
) -> Result<UnixListener, RemoteAuthorityHostError> {
    if socket_path.exists() {
        let _ = fs::remove_file(socket_path);
    }
    let listener = UnixListener::bind(socket_path)?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

fn spawn_output_ingest_thread(
    listener: UnixListener,
    running: Arc<AtomicBool>,
    tx: mpsc::Sender<AuthorityHostEvent>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        while running.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((mut stream, _)) => loop {
                    match read_output_chunk_frame(&mut stream) {
                        Ok(bytes) => {
                            if tx.send(AuthorityHostEvent::OutputChunk(bytes)).is_err() {
                                return;
                            }
                        }
                        Err(error) if error.is_unexpected_eof() => break,
                        Err(_) => break,
                    }
                },
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }
    })
}

fn pump_reader_to_ingest_socket(
    mut reader: impl Read,
    ingest_socket_path: &str,
) -> Result<(), RemoteAuthorityHostError> {
    let mut stream = UnixStream::connect(ingest_socket_path)?;
    let mut buffer = [0_u8; 4096];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        write_output_chunk_frame(&mut stream, &buffer[..read])?;
    }
    Ok(())
}

fn write_output_chunk_frame(
    writer: &mut impl Write,
    bytes: &[u8],
) -> Result<(), RemoteAuthorityHostError> {
    let len = u32::try_from(bytes.len())
        .map_err(|_| RemoteAuthorityHostError::new("output chunk exceeds u32 framing"))?;
    writer.write_all(&len.to_le_bytes())?;
    writer.write_all(bytes)?;
    writer.flush()?;
    Ok(())
}

fn read_output_chunk_frame(reader: &mut impl Read) -> Result<Vec<u8>, RemoteAuthorityHostError> {
    let mut len_bytes = [0_u8; 4];
    reader.read_exact(&mut len_bytes)?;
    let len = u32::from_le_bytes(len_bytes) as usize;
    let mut bytes = vec![0_u8; len];
    reader.read_exact(&mut bytes)?;
    Ok(bytes)
}

pub fn authority_output_ingest_socket_path(
    transport_socket_path: &str,
    target_id: &str,
) -> PathBuf {
    let hash = stable_socket_hash(&[transport_socket_path, target_id]);
    std::env::temp_dir().join(format!("waitagent-authority-output-{hash}.sock"))
}

fn remote_authority_output_pump_shell_command(
    executable: &str,
    ingest_socket_path: &Path,
) -> String {
    [
        shell_escape(executable),
        shell_escape("__remote-authority-output-pump"),
        shell_escape("--ingest-socket-path"),
        shell_escape(&ingest_socket_path.display().to_string()),
    ]
    .join(" ")
}

fn shell_escape(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct RemoteAuthorityHostError {
    message: String,
    io_kind: Option<io::ErrorKind>,
}

impl RemoteAuthorityHostError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            io_kind: None,
        }
    }

    fn is_unexpected_eof(&self) -> bool {
        self.io_kind == Some(io::ErrorKind::UnexpectedEof)
    }
}

impl fmt::Display for RemoteAuthorityHostError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for RemoteAuthorityHostError {}

impl From<io::Error> for RemoteAuthorityHostError {
    fn from(value: io::Error) -> Self {
        Self {
            message: value.to_string(),
            io_kind: Some(value.kind()),
        }
    }
}

fn remote_authority_error(error: impl ToString) -> LifecycleError {
    LifecycleError::Io(
        "failed to run remote authority target host".to_string(),
        io::Error::new(io::ErrorKind::Other, error.to_string()),
    )
}

fn wait_for_ready_socket(socket_path: &Path) -> Result<(), LifecycleError> {
    for _ in 0..100 {
        if socket_path.exists() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(10));
    }
    Err(LifecycleError::Protocol(format!(
        "authority live-session socket did not become ready at {}",
        socket_path.display()
    )))
}

#[cfg(test)]
mod tests {
    use super::{
        authority_output_ingest_socket_path, pump_reader_to_ingest_socket, read_output_chunk_frame,
        remote_authority_error, remote_authority_output_pump_shell_command,
        write_output_chunk_frame, LifecycleError, RemoteAuthorityPublicationGateway,
        RemoteAuthorityTargetHostRuntime, RemoteTargetPtyGateway,
    };
    use crate::cli::RemoteAuthorityTargetHostCommand;
    use crate::infra::remote_protocol::{
        ApplyResizePayload, ClientHelloPayload, ControlPlanePayload, NodeSessionChannel,
        NodeSessionEnvelope, ProtocolEnvelope, TargetExitedPayload, TargetInputPayload,
        TargetOutputPayload, TargetPublishedPayload,
    };
    use crate::infra::remote_transport_codec::{
        read_control_plane_envelope, read_node_session_envelope, write_node_session_envelope,
    };
    use crate::infra::tmux::TmuxPaneId;
    use crate::runtime::remote_node_session_owner_runtime::{
        live_authority_session_socket_path, spawn_live_authority_session_bridge,
    };
    use crate::runtime::remote_node_session_runtime::RemoteNodeSessionRuntime;
    use crate::runtime::remote_node_transport_runtime::write_server_hello;
    use crate::runtime::remote_target_publication_runtime::PublicationSenderCommand;
    use std::fs;
    use std::io::Cursor;
    use std::net::Shutdown;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::{Path, PathBuf};
    use std::process;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[derive(Clone, Default)]
    struct FakeGateway {
        input_calls: Arc<Mutex<Vec<Vec<u8>>>>,
        resize_calls: Arc<Mutex<Vec<(usize, usize)>>>,
        pipe_calls: Arc<Mutex<Vec<String>>>,
        clear_calls: Arc<Mutex<usize>>,
    }

    impl RemoteTargetPtyGateway for FakeGateway {
        type Error = &'static str;

        fn target_main_pane(
            &self,
            _socket_name: &str,
            _target_session_name: &str,
        ) -> Result<TmuxPaneId, Self::Error> {
            Ok(TmuxPaneId::new("%7"))
        }

        fn send_input(
            &self,
            _socket_name: &str,
            _pane: &TmuxPaneId,
            bytes: &[u8],
        ) -> Result<(), Self::Error> {
            self.input_calls
                .lock()
                .expect("input calls mutex should not be poisoned")
                .push(bytes.to_vec());
            Ok(())
        }

        fn resize_pty(
            &self,
            _socket_name: &str,
            _pane: &TmuxPaneId,
            cols: usize,
            rows: usize,
        ) -> Result<(), Self::Error> {
            self.resize_calls
                .lock()
                .expect("resize calls mutex should not be poisoned")
                .push((cols, rows));
            Ok(())
        }

        fn clear_output_pipe(
            &self,
            _socket_name: &str,
            _pane: &TmuxPaneId,
        ) -> Result<(), Self::Error> {
            let mut clear_calls = self
                .clear_calls
                .lock()
                .expect("clear calls mutex should not be poisoned");
            *clear_calls += 1;
            Ok(())
        }

        fn set_output_pipe(
            &self,
            _socket_name: &str,
            _pane: &TmuxPaneId,
            command: &str,
        ) -> Result<(), Self::Error> {
            self.pipe_calls
                .lock()
                .expect("pipe calls mutex should not be poisoned")
                .push(command.to_string());
            Ok(())
        }
    }

    struct FakeLiveSession {
        session: Arc<RemoteNodeSessionRuntime>,
        socket_path: PathBuf,
        running: Arc<AtomicBool>,
    }

    #[derive(Clone, Default)]
    struct FakePublicationGateway {
        live_session: Arc<Mutex<Option<FakeLiveSession>>>,
    }

    impl RemoteAuthorityPublicationGateway for FakePublicationGateway {
        fn ensure_live_session_registered(
            &self,
            socket_name: &str,
            target_session_name: &str,
            authority_id: &str,
            _target_id: &str,
            transport_socket_path: &str,
        ) -> Result<PathBuf, LifecycleError> {
            let session = Arc::new(
                RemoteNodeSessionRuntime::connect(transport_socket_path, authority_id, None)
                    .map_err(remote_authority_error)?,
            );
            let running = Arc::new(AtomicBool::new(true));
            let socket_path = live_authority_session_socket_path(socket_name, target_session_name);
            spawn_live_authority_session_bridge(
                socket_path.clone(),
                session.clone(),
                running.clone(),
            );
            super::wait_for_ready_socket(&socket_path)?;
            *self
                .live_session
                .lock()
                .expect("live session mutex should not be poisoned") = Some(FakeLiveSession {
                session,
                socket_path: socket_path.clone(),
                running,
            });
            Ok(socket_path)
        }

        fn ensure_live_session_unregistered(
            &self,
            _socket_name: &str,
            target_session_name: &str,
        ) -> Result<(), LifecycleError> {
            assert_eq!(target_session_name, "target-1");
            if let Some(live_session) = self
                .live_session
                .lock()
                .expect("live session mutex should not be poisoned")
                .take()
            {
                live_session.running.store(false, Ordering::Relaxed);
                live_session.session.shutdown();
                let _ = UnixStream::connect(&live_session.socket_path);
                let _ = fs::remove_file(live_session.socket_path);
            }
            Ok(())
        }

        fn start_source_publication(
            &self,
            socket_name: &str,
            target_session_name: &str,
        ) -> Result<(), LifecycleError> {
            self.live_session
                .lock()
                .expect("live session mutex should not be poisoned")
                .as_ref()
                .expect("live session should be registered")
                .session
                .send_publication_sender_command(&PublicationSenderCommand::PublishTarget {
                    authority_id: "peer-a".to_string(),
                    transport_session_id: "target-1".to_string(),
                    source_session_name: Some(target_session_name.to_string()),
                    selector: Some(format!("{socket_name}:{target_session_name}")),
                    availability: "online",
                    session_role: Some("target-host"),
                    workspace_key: Some("wk-1".to_string()),
                    command_name: Some("codex".to_string()),
                    current_path: Some("/tmp/demo".to_string()),
                    attached_clients: 1,
                    window_count: 1,
                })
                .map_err(remote_authority_error)?;
            Ok(())
        }

        fn stop_source_publication(
            &self,
            _socket_name: &str,
            target_session_name: &str,
        ) -> Result<(), LifecycleError> {
            self.live_session
                .lock()
                .expect("live session mutex should not be poisoned")
                .as_ref()
                .expect("live session should be registered")
                .session
                .send_publication_sender_command(&PublicationSenderCommand::ExitTarget {
                    authority_id: "peer-a".to_string(),
                    transport_session_id: "target-1".to_string(),
                    source_session_name: Some(target_session_name.to_string()),
                })
                .map_err(remote_authority_error)?;
            Ok(())
        }
    }

    #[test]
    fn authority_output_pump_shell_command_quotes_ingest_socket_path() {
        let command = remote_authority_output_pump_shell_command(
            "/tmp/wait agent",
            Path::new("/tmp/output path.sock"),
        );

        assert_eq!(
            command,
            "'/tmp/wait agent' '__remote-authority-output-pump' '--ingest-socket-path' '/tmp/output path.sock'"
        );
    }

    #[test]
    fn output_pump_reader_forwards_framed_chunks() {
        let socket_path = ingest_socket_path("pump");
        let listener = UnixListener::bind(&socket_path).expect("listener should bind");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("listener should accept");
            read_output_chunk_frame(&mut stream).expect("frame should decode")
        });

        pump_reader_to_ingest_socket(
            Cursor::new(b"hello".to_vec()),
            socket_path.to_string_lossy().as_ref(),
        )
        .expect("pump should forward bytes");

        let bytes = server.join().expect("server should join cleanly");
        assert_eq!(bytes, b"hello");
        let _ = fs::remove_file(&socket_path);
    }

    #[test]
    fn authority_host_runtime_routes_transport_commands_into_gateway_and_output_back_to_transport()
    {
        let socket_name = unique_test_socket_name("wa-1");
        let transport_socket_path = transport_socket_path("host");
        let transport_listener =
            UnixListener::bind(&transport_socket_path).expect("transport listener should bind");
        let fake_gateway = FakeGateway::default();
        let fake_publication_gateway = FakePublicationGateway::default();
        let runtime = RemoteAuthorityTargetHostRuntime::new(
            fake_gateway.clone(),
            fake_publication_gateway,
            PathBuf::from("/tmp/waitagent"),
        );
        let command = RemoteAuthorityTargetHostCommand {
            socket_name: socket_name.clone(),
            target_session_name: "target-1".to_string(),
            authority_id: "peer-a".to_string(),
            target_id: "remote-peer:peer-a:target-1".to_string(),
            transport_socket_path: transport_socket_path.to_string_lossy().into_owned(),
        };
        let ingest_socket_path = authority_output_ingest_socket_path(
            command.transport_socket_path.as_str(),
            &command.target_id,
        );
        let server = thread::spawn(move || {
            let (mut stream, _) = transport_listener
                .accept()
                .expect("transport should accept");
            let hello = read_control_plane_envelope(&mut stream).expect("hello should decode");
            let registered = match hello.payload {
                ControlPlanePayload::ClientHello(ClientHelloPayload { node_id, .. }) => node_id,
                other => panic!("unexpected hello payload: {other:?}"),
            };
            assert_eq!(registered, "peer-a");
            write_server_hello(&mut stream, "waitagent-remote-node-session")
                .expect("server hello should encode");
            write_node_session_envelope(
                &mut stream,
                &NodeSessionEnvelope {
                    channel: NodeSessionChannel::Authority,
                    envelope: target_input_envelope(),
                },
            )
            .expect("target input should encode");
            write_node_session_envelope(
                &mut stream,
                &NodeSessionEnvelope {
                    channel: NodeSessionChannel::Authority,
                    envelope: apply_resize_envelope(),
                },
            )
            .expect("apply resize should encode");
            let mut published_payload = None;
            let mut output_payload = None;
            let mut exited_payload = None;
            let mut write_shutdown = false;
            while published_payload.is_none()
                || output_payload.is_none()
                || exited_payload.is_none()
            {
                let envelope =
                    read_node_session_envelope(&mut stream).expect("node session should decode");
                match envelope.envelope.payload {
                    payload @ ControlPlanePayload::TargetPublished(_) => {
                        if published_payload.is_none() {
                            published_payload = Some(payload);
                        }
                    }
                    payload @ ControlPlanePayload::TargetOutput(_) => {
                        if output_payload.is_none() {
                            output_payload = Some(payload);
                        }
                    }
                    payload @ ControlPlanePayload::TargetExited(_) => {
                        exited_payload = Some(payload);
                    }
                    other => panic!("unexpected node-session payload: {other:?}"),
                }
                if !write_shutdown && published_payload.is_some() && output_payload.is_some() {
                    stream
                        .shutdown(Shutdown::Write)
                        .expect("server write shutdown should succeed");
                    write_shutdown = true;
                }
            }
            (
                published_payload.expect("published payload should be collected"),
                output_payload.expect("output payload should be collected"),
                exited_payload.expect("exit payload should be collected"),
            )
        });

        let runtime_thread = thread::spawn(move || runtime.run_target_host(command));

        wait_for_socket(&ingest_socket_path);
        let mut output_stream =
            UnixStream::connect(&ingest_socket_path).expect("ingest socket should accept");
        write_output_chunk_frame(&mut output_stream, b"hello").expect("output chunk should encode");
        drop(output_stream);

        let (published, output, exited) = server.join().expect("server should join cleanly");
        runtime_thread
            .join()
            .expect("runtime thread should join cleanly")
            .expect("runtime should finish cleanly");

        assert_eq!(
            fake_gateway
                .input_calls
                .lock()
                .expect("input calls mutex should not be poisoned")
                .clone(),
            vec![b"a".to_vec()]
        );
        assert_eq!(
            fake_gateway
                .resize_calls
                .lock()
                .expect("resize calls mutex should not be poisoned")
                .clone(),
            vec![(160, 50)]
        );
        match published {
            ControlPlanePayload::TargetPublished(TargetPublishedPayload {
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
            }) => {
                assert_eq!(transport_session_id, "target-1");
                assert_eq!(source_session_name.as_deref(), Some("target-1"));
                assert_eq!(selector, Some(format!("{socket_name}:target-1")));
                assert_eq!(availability, "online");
                assert_eq!(session_role, Some("target-host"));
                assert_eq!(workspace_key.as_deref(), Some("wk-1"));
                assert_eq!(command_name.as_deref(), Some("codex"));
                assert_eq!(current_path.as_deref(), Some("/tmp/demo"));
                assert_eq!(attached_clients, 1);
                assert_eq!(window_count, 1);
            }
            other => panic!("unexpected publication payload: {other:?}"),
        }
        match output {
            ControlPlanePayload::TargetOutput(TargetOutputPayload {
                output_seq,
                bytes_base64,
                ..
            }) => {
                assert_eq!(output_seq, 1);
                assert_eq!(bytes_base64, "aGVsbG8=");
            }
            other => panic!("unexpected authority output payload: {other:?}"),
        }
        match exited {
            ControlPlanePayload::TargetExited(TargetExitedPayload {
                transport_session_id,
                source_session_name,
            }) => {
                assert_eq!(transport_session_id, "target-1");
                assert_eq!(source_session_name.as_deref(), Some("target-1"));
            }
            other => panic!("unexpected exit payload: {other:?}"),
        }
        assert!(fake_gateway
            .pipe_calls
            .lock()
            .expect("pipe calls mutex should not be poisoned")[0]
            .contains("__remote-authority-output-pump"));
        let _ = fs::remove_file(&transport_socket_path);
    }

    #[test]
    fn authority_output_ingest_socket_path_scopes_by_transport_and_target() {
        let path = authority_output_ingest_socket_path(
            "/tmp/waitagent-remote-wa-1-workspace-1-peer-a_shell-1.sock",
            "remote-peer:peer-a:shell-1",
        );

        let rendered = path.to_string_lossy();
        assert!(rendered.contains("waitagent-authority-output-"));
        assert!(rendered.ends_with(".sock"));
    }

    fn wait_for_socket(path: &Path) {
        for _ in 0..100 {
            if path.exists() {
                return;
            }
            thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("socket did not appear at {}", path.display());
    }

    fn transport_socket_path(name: &str) -> PathBuf {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        std::env::temp_dir().join(format!(
            "waitagent-test-authority-transport-{name}-{}-{millis}.sock",
            process::id()
        ))
    }

    fn ingest_socket_path(name: &str) -> PathBuf {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        std::env::temp_dir().join(format!(
            "waitagent-test-authority-ingest-{name}-{}-{millis}.sock",
            process::id()
        ))
    }

    fn unique_test_socket_name(prefix: &str) -> String {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        format!("{prefix}-{}-{millis}", process::id())
    }

    fn target_input_envelope() -> ProtocolEnvelope<ControlPlanePayload> {
        ProtocolEnvelope {
            protocol_version: "1.1".to_string(),
            message_id: "msg-target-input".to_string(),
            message_type: "target_input",
            timestamp: "2026-04-28T00:00:00Z".to_string(),
            sender_id: "server".to_string(),
            correlation_id: None,
            target_id: Some("remote-peer:peer-a:target-1".to_string()),
            attachment_id: Some("attach-1".to_string()),
            console_id: Some("console-a".to_string()),
            payload: ControlPlanePayload::TargetInput(TargetInputPayload {
                attachment_id: "attach-1".to_string(),
                target_id: "remote-peer:peer-a:target-1".to_string(),
                console_id: "console-a".to_string(),
                console_host_id: "observer-a".to_string(),
                input_seq: 1,
                bytes_base64: "YQ==".to_string(),
            }),
        }
    }

    fn apply_resize_envelope() -> ProtocolEnvelope<ControlPlanePayload> {
        ProtocolEnvelope {
            protocol_version: "1.1".to_string(),
            message_id: "msg-apply-resize".to_string(),
            message_type: "apply_resize",
            timestamp: "2026-04-28T00:00:00Z".to_string(),
            sender_id: "server".to_string(),
            correlation_id: None,
            target_id: Some("remote-peer:peer-a:target-1".to_string()),
            attachment_id: Some("attach-1".to_string()),
            console_id: Some("console-a".to_string()),
            payload: ControlPlanePayload::ApplyResize(ApplyResizePayload {
                target_id: "remote-peer:peer-a:target-1".to_string(),
                resize_epoch: 2,
                resize_authority_console_id: "console-a".to_string(),
                cols: 160,
                rows: 50,
            }),
        }
    }
}
