use crate::cli::{RemoteAuthorityOutputPumpCommand, RemoteAuthorityTargetHostCommand};
use crate::infra::base64::{decode_base64, encode_base64};
use crate::infra::tmux::{EmbeddedTmuxBackend, TmuxError, TmuxPaneId};
use crate::lifecycle::LifecycleError;
use crate::runtime::remote_authority_transport_runtime::{
    RemoteAuthorityCommand, RemoteAuthorityTransportRuntime,
};
use crate::runtime::remote_target_publication_runtime::{
    ensure_publication_owner_process_running, signal_publication_owner_refresh,
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

pub struct RemoteAuthorityTargetHostRuntime<G = EmbeddedTmuxBackend> {
    gateway: G,
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

impl RemoteAuthorityTargetHostRuntime<EmbeddedTmuxBackend> {
    pub fn from_build_env() -> Result<Self, LifecycleError> {
        let gateway = EmbeddedTmuxBackend::from_build_env().map_err(remote_authority_error)?;
        let current_executable = std::env::current_exe().map_err(|error| {
            LifecycleError::Io(
                "failed to locate current waitagent executable".to_string(),
                error,
            )
        })?;
        Ok(Self::new(gateway, current_executable))
    }
}

impl<G> RemoteAuthorityTargetHostRuntime<G>
where
    G: RemoteTargetPtyGateway,
{
    pub fn new(gateway: G, current_executable: PathBuf) -> Self {
        Self {
            gateway,
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
        let transport = Arc::new(
            RemoteAuthorityTransportRuntime::connect(
                &command.transport_socket_path,
                &command.authority_id,
            )
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
        let _ = ensure_publication_owner_process_running(
            &self.current_executable,
            &command.socket_name,
            &command.target_session_name,
        )
        .and_then(|()| {
            signal_publication_owner_refresh(&command.socket_name, &command.target_session_name)
        });
        let _output_guard = OutputPipeGuard {
            gateway: self.gateway.clone(),
            socket_name: command.socket_name.clone(),
            pane: pane.clone(),
            ingest_socket_path: ingest_socket_path.clone(),
        };

        let (event_tx, event_rx) = mpsc::channel();
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

        loop {
            match event_rx.recv() {
                Ok(AuthorityHostEvent::TransportCommand(RemoteAuthorityCommand::TargetInput(
                    payload,
                ))) => {
                    let bytes =
                        decode_base64(&payload.bytes_base64).map_err(remote_authority_error)?;
                    self.gateway
                        .send_input(&command.socket_name, &pane, &bytes)
                        .map_err(remote_authority_error)?;
                }
                Ok(AuthorityHostEvent::TransportCommand(RemoteAuthorityCommand::ApplyResize(
                    payload,
                ))) => {
                    self.gateway
                        .resize_pty(&command.socket_name, &pane, payload.cols, payload.rows)
                        .map_err(remote_authority_error)?;
                }
                Ok(AuthorityHostEvent::OutputChunk(bytes)) => {
                    output_seq += 1;
                    transport
                        .send_target_output(
                            &command.target_id,
                            output_seq,
                            "pty",
                            encode_base64(&bytes),
                        )
                        .map_err(remote_authority_error)?;
                }
                Ok(AuthorityHostEvent::TransportClosed) | Err(_) => break,
            }
        }

        running.store(false, Ordering::Relaxed);
        let _ = UnixStream::connect(&ingest_socket_path);
        let _ = command_thread.join();
        let _ = output_thread.join();
        Ok(())
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

#[cfg(test)]
mod tests {
    use super::{
        authority_output_ingest_socket_path, pump_reader_to_ingest_socket, read_output_chunk_frame,
        remote_authority_output_pump_shell_command, write_output_chunk_frame,
        RemoteAuthorityTargetHostRuntime, RemoteTargetPtyGateway,
    };
    use crate::cli::RemoteAuthorityTargetHostCommand;
    use crate::infra::remote_protocol::{
        ApplyResizePayload, ControlPlanePayload, ProtocolEnvelope, TargetInputPayload,
    };
    use crate::infra::remote_transport_codec::{
        read_control_plane_envelope, read_registration_frame, write_control_plane_envelope,
    };
    use crate::infra::tmux::TmuxPaneId;
    use std::fs;
    use std::io::Cursor;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::{Path, PathBuf};
    use std::process;
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
        let transport_socket_path = transport_socket_path("host");
        let transport_listener =
            UnixListener::bind(&transport_socket_path).expect("transport listener should bind");
        let fake_gateway = FakeGateway::default();
        let runtime = RemoteAuthorityTargetHostRuntime::new(
            fake_gateway.clone(),
            PathBuf::from("/tmp/waitagent"),
        );
        let command = RemoteAuthorityTargetHostCommand {
            socket_name: "wa-1".to_string(),
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
            let registered =
                read_registration_frame(&mut stream).expect("registration should decode");
            assert_eq!(registered, "peer-a");
            write_control_plane_envelope(&mut stream, &target_input_envelope())
                .expect("target input should encode");
            write_control_plane_envelope(&mut stream, &apply_resize_envelope())
                .expect("apply resize should encode");
            let envelope =
                read_control_plane_envelope(&mut stream).expect("target output should decode");
            match envelope.payload {
                ControlPlanePayload::TargetOutput(payload) => payload,
                other => panic!("unexpected payload: {other:?}"),
            }
        });

        let runtime_thread = thread::spawn(move || runtime.run_target_host(command));

        wait_for_socket(&ingest_socket_path);
        let mut output_stream =
            UnixStream::connect(&ingest_socket_path).expect("ingest socket should accept");
        write_output_chunk_frame(&mut output_stream, b"hello").expect("output chunk should encode");
        drop(output_stream);

        let payload = server.join().expect("server should join cleanly");
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
        assert_eq!(payload.output_seq, 1);
        assert_eq!(payload.bytes_base64, "aGVsbG8=");
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
