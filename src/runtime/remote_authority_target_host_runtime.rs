use crate::cli::{
    prepend_global_network_args, RemoteAuthorityOutputPumpCommand,
    RemoteAuthorityTargetHostCommand, RemoteNetworkConfig,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MirrorState {
    Inactive,
    Active,
}

pub trait RemoteTargetPtyGateway: Send + Sync + Clone + 'static {
    type Error: ToString;

    fn target_presentation_pane(
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

    fn capture_bootstrap_screen(
        &self,
        socket_name: &str,
        pane: &TmuxPaneId,
    ) -> Result<String, Self::Error>;

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

    fn target_presentation_pane(
        &self,
        socket_name: &str,
        target_session_name: &str,
    ) -> Result<TmuxPaneId, Self::Error> {
        self.target_presentation_pane_on_socket(socket_name, target_session_name)
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

    fn capture_bootstrap_screen(
        &self,
        socket_name: &str,
        pane: &TmuxPaneId,
    ) -> Result<String, Self::Error> {
        self.capture_pane_ansi_on_socket(socket_name, pane.as_str())
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
            .target_presentation_pane(&command.socket_name, &command.target_session_name)
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
        let mut mirror_state = MirrorState::Inactive;

        let loop_result = loop {
            match event_rx.recv() {
                Ok(AuthorityHostEvent::TransportCommand(RemoteAuthorityCommand::OpenMirror(
                    payload,
                ))) => {
                    if mirror_state == MirrorState::Active {
                        if let Err(error) = self
                            .gateway
                            .resize_pty(&command.socket_name, &pane, payload.cols, payload.rows)
                            .map_err(remote_authority_error)
                        {
                            break Err(error);
                        }
                        if let Err(error) = transport
                            .send_open_mirror_accepted(
                                &payload.session_id,
                                &payload.target_id,
                                "online",
                            )
                            .map_err(remote_authority_error)
                        {
                            break Err(error);
                        }
                        if let Err(error) = emit_bootstrap(
                            self,
                            &command.socket_name,
                            &pane,
                            &transport,
                            &command.transport_session_id,
                            &command.target_id,
                        ) {
                            break Err(error);
                        }
                        continue;
                    }
                    if payload.target_id != command.target_id
                        || payload.session_id != command.transport_session_id
                    {
                        if let Err(error) = transport
                            .send_open_mirror_rejected(
                                &payload.session_id,
                                &payload.target_id,
                                "mirror_not_available",
                                "requested session does not match local target host",
                            )
                            .map_err(remote_authority_error)
                        {
                            break Err(error);
                        }
                        continue;
                    }
                    if let Err(error) =
                        activate_mirror(self, &command, &pane, &ingest_socket_path, &payload)
                    {
                        if transport
                            .send_open_mirror_rejected(
                                &payload.session_id,
                                &payload.target_id,
                                "mirror_not_available",
                                error.to_string(),
                            )
                            .is_err()
                        {
                            break Err(error);
                        }
                        continue;
                    }
                    mirror_state = MirrorState::Active;
                    if let Err(error) = transport
                        .send_open_mirror_accepted(
                            &payload.session_id,
                            &payload.target_id,
                            "online",
                        )
                        .map_err(remote_authority_error)
                    {
                        break Err(error);
                    }
                    if let Err(error) = emit_bootstrap(
                        self,
                        &command.socket_name,
                        &pane,
                        &transport,
                        &command.transport_session_id,
                        &command.target_id,
                    ) {
                        break Err(error);
                    }
                }
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
                Ok(AuthorityHostEvent::TransportCommand(RemoteAuthorityCommand::CloseMirror(
                    _payload,
                ))) => {
                    if mirror_state == MirrorState::Active {
                        if let Err(error) = deactivate_mirror(self, &command, &pane) {
                            break Err(error);
                        }
                        mirror_state = MirrorState::Inactive;
                    }
                }
                Ok(AuthorityHostEvent::OutputChunk(bytes)) => {
                    if mirror_state != MirrorState::Active {
                        continue;
                    }
                    output_seq += 1;
                    if let Err(error) = transport
                        .send_target_output(
                            &command.transport_session_id,
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
                    if mirror_state == MirrorState::Active {
                        if let Err(error) = deactivate_mirror(self, &command, &pane) {
                            break Err(error);
                        }
                    }
                    break Ok(());
                }
                Err(_) => break Ok(()),
            }
        };

        if mirror_state == MirrorState::Active {
            let _ = deactivate_mirror(self, &command, &pane);
        }
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

pub(crate) fn remote_authority_target_host_args(
    socket_name: &str,
    target_session_name: &str,
    transport_session_id: &str,
    authority_id: &str,
    target_id: &str,
    transport_socket_path: &str,
    network: &RemoteNetworkConfig,
) -> Vec<String> {
    prepend_global_network_args(
        vec![
            "__remote-authority-target-host".to_string(),
            "--socket-name".to_string(),
            socket_name.to_string(),
            "--target-session-name".to_string(),
            target_session_name.to_string(),
            "--transport-session-id".to_string(),
            transport_session_id.to_string(),
            "--authority-id".to_string(),
            authority_id.to_string(),
            "--target-id".to_string(),
            target_id.to_string(),
            "--transport-socket-path".to_string(),
            transport_socket_path.to_string(),
        ],
        network,
    )
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

fn activate_mirror<G, P>(
    runtime: &RemoteAuthorityTargetHostRuntime<G, P>,
    command: &RemoteAuthorityTargetHostCommand,
    pane: &TmuxPaneId,
    ingest_socket_path: &Path,
    payload: &crate::infra::remote_protocol::OpenMirrorRequestPayload,
) -> Result<(), LifecycleError>
where
    G: RemoteTargetPtyGateway,
    P: RemoteAuthorityPublicationGateway,
{
    let pipe_command = remote_authority_output_pump_shell_command(
        runtime.current_executable.to_string_lossy().as_ref(),
        ingest_socket_path,
    );
    runtime
        .gateway
        .clear_output_pipe(&command.socket_name, pane)
        .map_err(remote_authority_error)?;
    runtime
        .gateway
        .set_output_pipe(&command.socket_name, pane, &pipe_command)
        .map_err(remote_authority_error)?;
    runtime
        .gateway
        .resize_pty(&command.socket_name, pane, payload.cols, payload.rows)
        .map_err(remote_authority_error)?;
    Ok(())
}

fn emit_bootstrap<G, P>(
    runtime: &RemoteAuthorityTargetHostRuntime<G, P>,
    socket_name: &str,
    pane: &TmuxPaneId,
    transport: &RemoteAuthorityTransportRuntime,
    session_id: &str,
    target_id: &str,
) -> Result<(), LifecycleError>
where
    G: RemoteTargetPtyGateway,
    P: RemoteAuthorityPublicationGateway,
{
    let screen = runtime
        .gateway
        .capture_bootstrap_screen(socket_name, pane)
        .map_err(remote_authority_error)?;
    if !screen.is_empty() {
        transport
            .send_mirror_bootstrap_chunk(
                session_id,
                target_id,
                1,
                "pty",
                encode_base64(screen.as_bytes()),
            )
            .map_err(remote_authority_error)?;
    }
    transport
        .send_mirror_bootstrap_complete(
            session_id,
            target_id,
            if screen.is_empty() { 0 } else { 1 },
        )
        .map_err(remote_authority_error)?;
    Ok(())
}

fn deactivate_mirror<G, P>(
    runtime: &RemoteAuthorityTargetHostRuntime<G, P>,
    command: &RemoteAuthorityTargetHostCommand,
    pane: &TmuxPaneId,
) -> Result<(), LifecycleError>
where
    G: RemoteTargetPtyGateway,
    P: RemoteAuthorityPublicationGateway,
{
    runtime
        .gateway
        .clear_output_pipe(&command.socket_name, pane)
        .map_err(remote_authority_error)?;
    Ok(())
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
        remote_authority_target_host_args, write_output_chunk_frame, LifecycleError,
        RemoteAuthorityPublicationGateway, RemoteAuthorityTargetHostRuntime,
        RemoteTargetPtyGateway,
    };
    use crate::cli::RemoteAuthorityTargetHostCommand;
    use crate::cli::RemoteNetworkConfig;
    use crate::infra::base64::decode_base64;
    use crate::infra::remote_protocol::{
        ApplyResizePayload, ClientHelloPayload, ControlPlanePayload, NodeSessionChannel,
        NodeSessionEnvelope, OpenMirrorRequestPayload, ProtocolEnvelope, TargetInputPayload,
        TargetOutputPayload,
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
        capture_bootstrap_screen: Arc<Mutex<String>>,
    }

    impl RemoteTargetPtyGateway for FakeGateway {
        type Error = &'static str;

        fn target_presentation_pane(
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

        fn capture_bootstrap_screen(
            &self,
            _socket_name: &str,
            _pane: &TmuxPaneId,
        ) -> Result<String, Self::Error> {
            Ok(self
                .capture_bootstrap_screen
                .lock()
                .expect("capture bootstrap screen mutex should not be poisoned")
                .clone())
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
    fn authority_target_host_args_include_network_and_route_metadata() {
        let args = remote_authority_target_host_args(
            "wa-1",
            "target-1",
            "shell-1",
            "peer-a",
            "remote-peer:peer-a:target-1",
            "/tmp/transport.sock",
            &RemoteNetworkConfig {
                port: 9001,
                connect: Some("10.0.0.8:7474".to_string()),
            },
        );

        assert_eq!(
            args,
            vec![
                "--port",
                "9001",
                "--connect",
                "10.0.0.8:7474",
                "__remote-authority-target-host",
                "--socket-name",
                "wa-1",
                "--target-session-name",
                "target-1",
                "--transport-session-id",
                "shell-1",
                "--authority-id",
                "peer-a",
                "--target-id",
                "remote-peer:peer-a:target-1",
                "--transport-socket-path",
                "/tmp/transport.sock",
            ]
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
            transport_session_id: "target-1".to_string(),
            authority_id: "peer-a".to_string(),
            target_id: "remote-peer:peer-a:target-1".to_string(),
            transport_socket_path: transport_socket_path.to_string_lossy().into_owned(),
        };
        let ingest_socket_path = authority_output_ingest_socket_path(
            command.transport_socket_path.as_str(),
            &command.target_id,
        );
        let server_ingest_socket_path = ingest_socket_path.clone();
        let (server_tx, server_rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let (mut stream, _) = transport_listener
                .accept()
                .expect("transport should accept");
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(1)))
                .expect("transport stream should accept read timeout");
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
                    envelope: open_mirror_envelope(),
                },
            )
            .expect("open mirror should encode");
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
            let mut output_payload = None;
            let mut accepted_payload = None;
            let mut bootstrap_chunk_payload = None;
            let mut bootstrap_complete_payload = None;
            while accepted_payload.is_none()
                || bootstrap_complete_payload.is_none()
                || output_payload.is_none()
            {
                let envelope = read_node_session_envelope(&mut stream).unwrap_or_else(|error| {
                    panic!(
                        "node session should decode while waiting for accepted/bootstrap/output; accepted={} bootstrap_complete={} output={} error={error:?}",
                        accepted_payload.is_some(),
                        bootstrap_complete_payload.is_some(),
                        output_payload.is_some(),
                    )
                });
                match envelope.envelope.payload {
                    payload @ ControlPlanePayload::OpenMirrorAccepted(_) => {
                        if accepted_payload.is_none() {
                            accepted_payload = Some(payload);
                            wait_for_socket(&server_ingest_socket_path);
                            let mut output_stream = UnixStream::connect(&server_ingest_socket_path)
                                .expect("ingest socket should accept");
                            write_output_chunk_frame(&mut output_stream, b"hello")
                                .expect("output chunk should encode");
                            drop(output_stream);
                        }
                    }
                    payload @ ControlPlanePayload::MirrorBootstrapChunk(_) => {
                        if bootstrap_chunk_payload.is_none() {
                            bootstrap_chunk_payload = Some(payload);
                        }
                    }
                    payload @ ControlPlanePayload::MirrorBootstrapComplete(_) => {
                        if bootstrap_complete_payload.is_none() {
                            bootstrap_complete_payload = Some(payload);
                        }
                    }
                    payload @ ControlPlanePayload::TargetOutput(_) => {
                        if output_payload.is_none() {
                            output_payload = Some(payload);
                            write_node_session_envelope(
                                &mut stream,
                                &NodeSessionEnvelope {
                                    channel: NodeSessionChannel::Authority,
                                    envelope: close_mirror_envelope(),
                                },
                            )
                            .expect("close mirror should encode");
                        }
                    }
                    other => panic!("unexpected node-session payload: {other:?}"),
                }
            }
            stream
                .shutdown(Shutdown::Write)
                .expect("server shutdown should succeed");
            let _ = server_tx.send((
                accepted_payload.expect("accepted payload should be collected"),
                bootstrap_chunk_payload,
                bootstrap_complete_payload.expect("bootstrap complete payload should be collected"),
                output_payload.expect("output payload should be collected"),
            ));
        });

        let (runtime_tx, runtime_rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let _ = runtime_tx.send(runtime.run_target_host(command));
        });

        let (accepted, bootstrap_chunk, bootstrap_complete, output) = server_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("server harness should complete within timeout");
        runtime_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("runtime should complete within timeout")
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
            vec![(80, 24), (160, 50)]
        );
        assert_eq!(
            accepted,
            ControlPlanePayload::OpenMirrorAccepted(
                crate::infra::remote_protocol::OpenMirrorAcceptedPayload {
                    session_id: "target-1".to_string(),
                    target_id: "remote-peer:peer-a:target-1".to_string(),
                    availability: "online",
                }
            )
        );
        assert_eq!(bootstrap_chunk, None);
        assert_eq!(
            bootstrap_complete,
            ControlPlanePayload::MirrorBootstrapComplete(
                crate::infra::remote_protocol::MirrorBootstrapCompletePayload {
                    session_id: "target-1".to_string(),
                    target_id: "remote-peer:peer-a:target-1".to_string(),
                    last_chunk_seq: 0,
                }
            )
        );
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
        assert!(fake_gateway
            .pipe_calls
            .lock()
            .expect("pipe calls mutex should not be poisoned")[0]
            .contains("__remote-authority-output-pump"));
        let _ = fs::remove_file(&transport_socket_path);
    }

    #[test]
    fn authority_host_runtime_sends_bootstrap_screen_with_ansi_sequences() {
        let socket_name = unique_test_socket_name("wa-ansi");
        let transport_socket_path = transport_socket_path("host-ansi");
        let transport_listener =
            UnixListener::bind(&transport_socket_path).expect("transport listener should bind");
        let fake_gateway = FakeGateway {
            capture_bootstrap_screen: Arc::new(Mutex::new("\u{1b}[32mbash\u{1b}[0m".to_string())),
            ..FakeGateway::default()
        };
        let runtime = RemoteAuthorityTargetHostRuntime::new(
            fake_gateway,
            FakePublicationGateway::default(),
            PathBuf::from("/tmp/waitagent"),
        );
        let command = RemoteAuthorityTargetHostCommand {
            socket_name: socket_name.clone(),
            target_session_name: "target-1".to_string(),
            transport_session_id: "target-1".to_string(),
            authority_id: "peer-a".to_string(),
            target_id: "remote-peer:peer-a:target-1".to_string(),
            transport_socket_path: transport_socket_path.to_string_lossy().into_owned(),
        };
        let (server_tx, server_rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let (mut stream, _) = transport_listener
                .accept()
                .expect("transport should accept");
            let hello = read_control_plane_envelope(&mut stream).expect("hello should decode");
            match hello.payload {
                ControlPlanePayload::ClientHello(ClientHelloPayload { .. }) => {}
                other => panic!("unexpected hello payload: {other:?}"),
            }
            write_server_hello(&mut stream, "waitagent-remote-node-session")
                .expect("server hello should encode");
            write_node_session_envelope(
                &mut stream,
                &NodeSessionEnvelope {
                    channel: NodeSessionChannel::Authority,
                    envelope: open_mirror_envelope(),
                },
            )
            .expect("open mirror should encode");

            let mut bootstrap_chunk = None;
            let mut bootstrap_complete = None;
            while bootstrap_chunk.is_none() || bootstrap_complete.is_none() {
                let envelope =
                    read_node_session_envelope(&mut stream).expect("node session should decode");
                match envelope.envelope.payload {
                    payload @ ControlPlanePayload::MirrorBootstrapChunk(_) => {
                        if bootstrap_chunk.is_none() {
                            bootstrap_chunk = Some(payload);
                        }
                    }
                    payload @ ControlPlanePayload::MirrorBootstrapComplete(_) => {
                        if bootstrap_complete.is_none() {
                            bootstrap_complete = Some(payload);
                            write_node_session_envelope(
                                &mut stream,
                                &NodeSessionEnvelope {
                                    channel: NodeSessionChannel::Authority,
                                    envelope: close_mirror_envelope(),
                                },
                            )
                            .expect("close mirror should encode");
                        }
                    }
                    _ => {}
                }
            }
            stream
                .shutdown(Shutdown::Write)
                .expect("server shutdown should succeed");
            server_tx
                .send((
                    bootstrap_chunk.expect("bootstrap chunk should exist"),
                    bootstrap_complete.expect("bootstrap complete should exist"),
                ))
                .expect("bootstrap payloads should send");
        });

        runtime
            .run_target_host(command)
            .expect("runtime should finish cleanly");

        let (bootstrap_chunk, bootstrap_complete) = server_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("server harness should complete");

        match bootstrap_chunk {
            ControlPlanePayload::MirrorBootstrapChunk(payload) => {
                assert_eq!(
                    decode_base64(&payload.bytes_base64).expect("bootstrap payload should decode"),
                    b"\x1b[32mbash\x1b[0m"
                );
            }
            other => panic!("unexpected bootstrap payload: {other:?}"),
        }
        assert_eq!(
            bootstrap_complete,
            ControlPlanePayload::MirrorBootstrapComplete(
                crate::infra::remote_protocol::MirrorBootstrapCompletePayload {
                    session_id: "target-1".to_string(),
                    target_id: "remote-peer:peer-a:target-1".to_string(),
                    last_chunk_seq: 1,
                }
            )
        );
        let _ = fs::remove_file(&transport_socket_path);
    }

    #[test]
    fn authority_host_runtime_replays_bootstrap_for_repeated_open_mirror() {
        let socket_name = unique_test_socket_name("wa-reopen");
        let transport_socket_path = transport_socket_path("host-reopen");
        let transport_listener =
            UnixListener::bind(&transport_socket_path).expect("transport listener should bind");
        let fake_gateway = FakeGateway {
            capture_bootstrap_screen: Arc::new(Mutex::new("\u{1b}[32mbash\u{1b}[0m".to_string())),
            ..FakeGateway::default()
        };
        let runtime = RemoteAuthorityTargetHostRuntime::new(
            fake_gateway.clone(),
            FakePublicationGateway::default(),
            PathBuf::from("/tmp/waitagent"),
        );
        let command = RemoteAuthorityTargetHostCommand {
            socket_name: socket_name.clone(),
            target_session_name: "target-1".to_string(),
            transport_session_id: "target-1".to_string(),
            authority_id: "peer-a".to_string(),
            target_id: "remote-peer:peer-a:target-1".to_string(),
            transport_socket_path: transport_socket_path.to_string_lossy().into_owned(),
        };
        let (server_tx, server_rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let (mut stream, _) = transport_listener
                .accept()
                .expect("transport should accept");
            let hello = read_control_plane_envelope(&mut stream).expect("hello should decode");
            match hello.payload {
                ControlPlanePayload::ClientHello(ClientHelloPayload { .. }) => {}
                other => panic!("unexpected hello payload: {other:?}"),
            }
            write_server_hello(&mut stream, "waitagent-remote-node-session")
                .expect("server hello should encode");

            write_node_session_envelope(
                &mut stream,
                &NodeSessionEnvelope {
                    channel: NodeSessionChannel::Authority,
                    envelope: open_mirror_envelope(),
                },
            )
            .expect("first open mirror should encode");

            let mut accepted_count = 0usize;
            let mut bootstrap_chunk_count = 0usize;
            let mut bootstrap_complete_count = 0usize;
            while accepted_count < 2 || bootstrap_complete_count < 2 || bootstrap_chunk_count < 2 {
                let envelope =
                    read_node_session_envelope(&mut stream).expect("node session should decode");
                match envelope.envelope.payload {
                    ControlPlanePayload::OpenMirrorAccepted(_) => {
                        accepted_count += 1;
                        if accepted_count == 1 {
                            write_node_session_envelope(
                                &mut stream,
                                &NodeSessionEnvelope {
                                    channel: NodeSessionChannel::Authority,
                                    envelope: open_mirror_envelope(),
                                },
                            )
                            .expect("second open mirror should encode");
                        }
                    }
                    ControlPlanePayload::MirrorBootstrapChunk(payload) => {
                        bootstrap_chunk_count += 1;
                        assert_eq!(
                            decode_base64(&payload.bytes_base64)
                                .expect("bootstrap payload should decode"),
                            b"\x1b[32mbash\x1b[0m"
                        );
                    }
                    ControlPlanePayload::MirrorBootstrapComplete(payload) => {
                        bootstrap_complete_count += 1;
                        assert_eq!(payload.last_chunk_seq, 1);
                        if bootstrap_complete_count == 2 {
                            write_node_session_envelope(
                                &mut stream,
                                &NodeSessionEnvelope {
                                    channel: NodeSessionChannel::Authority,
                                    envelope: close_mirror_envelope(),
                                },
                            )
                            .expect("close mirror should encode");
                        }
                    }
                    other => panic!("unexpected node-session payload: {other:?}"),
                }
            }
            stream
                .shutdown(Shutdown::Write)
                .expect("server shutdown should succeed");
            server_tx
                .send((
                    accepted_count,
                    bootstrap_chunk_count,
                    bootstrap_complete_count,
                ))
                .expect("counts should send");
        });

        runtime
            .run_target_host(command)
            .expect("runtime should finish cleanly");

        let (accepted_count, bootstrap_chunk_count, bootstrap_complete_count) = server_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("server harness should complete");

        assert_eq!(accepted_count, 2);
        assert_eq!(bootstrap_chunk_count, 2);
        assert_eq!(bootstrap_complete_count, 2);
        assert_eq!(
            fake_gateway
                .resize_calls
                .lock()
                .expect("resize calls mutex should not be poisoned")
                .clone(),
            vec![(80, 24), (80, 24)]
        );
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
            session_id: Some("target-1".to_string()),
            target_id: Some("remote-peer:peer-a:target-1".to_string()),
            attachment_id: Some("attach-1".to_string()),
            console_id: Some("console-a".to_string()),
            payload: ControlPlanePayload::TargetInput(TargetInputPayload {
                attachment_id: "attach-1".to_string(),
                session_id: "target-1".to_string(),
                target_id: "remote-peer:peer-a:target-1".to_string(),
                console_id: "console-a".to_string(),
                console_host_id: "observer-a".to_string(),
                input_seq: 1,
                bytes_base64: "YQ==".to_string(),
            }),
        }
    }

    fn open_mirror_envelope() -> ProtocolEnvelope<ControlPlanePayload> {
        ProtocolEnvelope {
            protocol_version: "1.1".to_string(),
            message_id: "msg-open-mirror".to_string(),
            message_type: "open_mirror_request",
            timestamp: "2026-04-28T00:00:00Z".to_string(),
            sender_id: "server".to_string(),
            correlation_id: None,
            session_id: Some("target-1".to_string()),
            target_id: Some("remote-peer:peer-a:target-1".to_string()),
            attachment_id: None,
            console_id: Some("console-a".to_string()),
            payload: ControlPlanePayload::OpenMirrorRequest(OpenMirrorRequestPayload {
                session_id: "target-1".to_string(),
                target_id: "remote-peer:peer-a:target-1".to_string(),
                console_id: "console-a".to_string(),
                cols: 80,
                rows: 24,
            }),
        }
    }

    fn close_mirror_envelope() -> ProtocolEnvelope<ControlPlanePayload> {
        ProtocolEnvelope {
            protocol_version: "1.1".to_string(),
            message_id: "msg-close-mirror".to_string(),
            message_type: "close_mirror_request",
            timestamp: "2026-04-28T00:00:00Z".to_string(),
            sender_id: "server".to_string(),
            correlation_id: None,
            session_id: Some("target-1".to_string()),
            target_id: Some("remote-peer:peer-a:target-1".to_string()),
            attachment_id: None,
            console_id: Some("console-a".to_string()),
            payload: ControlPlanePayload::CloseMirrorRequest(
                crate::infra::remote_protocol::CloseMirrorRequestPayload {
                    session_id: "target-1".to_string(),
                    target_id: "remote-peer:peer-a:target-1".to_string(),
                },
            ),
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
            session_id: Some("target-1".to_string()),
            target_id: Some("remote-peer:peer-a:target-1".to_string()),
            attachment_id: Some("attach-1".to_string()),
            console_id: Some("console-a".to_string()),
            payload: ControlPlanePayload::ApplyResize(ApplyResizePayload {
                session_id: "target-1".to_string(),
                target_id: "remote-peer:peer-a:target-1".to_string(),
                resize_epoch: 2,
                resize_authority_console_id: "console-a".to_string(),
                cols: 160,
                rows: 50,
            }),
        }
    }
}
