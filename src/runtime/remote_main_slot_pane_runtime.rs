use crate::application::target_registry_service::{
    DefaultTargetCatalogGateway, TargetRegistryService,
};
use crate::cli::RemoteMainSlotCommand;
use crate::domain::session_catalog::{ConsoleLocation, ManagedSessionRecord, SessionTransport};
use crate::infra::base64::encode_base64;
use crate::infra::remote_protocol::{
    ControlPlanePayload, ProtocolEnvelope, RemoteConsoleDescriptor,
};
use crate::infra::remote_transport_codec::{
    read_control_plane_envelope, read_registration_frame, write_control_plane_envelope,
    RemoteTransportCodecError,
};
use crate::lifecycle::LifecycleError;
use crate::runtime::remote_authority_transport_runtime::authority_transport_socket_path;
use crate::runtime::remote_main_slot_runtime::{
    RemoteAttachmentBinding, RemoteControlPlaneTransportError, RemoteMainSlotRuntime,
};
use crate::runtime::remote_observer_runtime::{RemoteObserverRuntime, RemoteObserverSnapshot};
use crate::runtime::remote_transport_runtime::{
    LocalNodeMailbox, RemoteConnectionRegistry, RemoteControlPlaneConnection,
};
use crate::terminal::{TerminalRuntime, TerminalSize};
use std::fmt;
use std::fs;
use std::io::{self, Read, Write};
use std::os::raw::{c_int, c_void};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;

const SIGWINCH: c_int = 28;
const HIDE_CURSOR_ESCAPE: &str = "\x1b[?25l";
const SHOW_CURSOR_ESCAPE: &str = "\x1b[?25h";

static REMOTE_PANE_SIGWINCH_WRITE_FD: AtomicI32 = AtomicI32::new(-1);

extern "C" {
    fn signal(signum: c_int, handler: extern "C" fn(c_int)) -> usize;
    fn write(fd: c_int, buf: *const c_void, count: usize) -> isize;
}

pub struct RemoteMainSlotPaneRuntime {
    target_registry: TargetRegistryService<DefaultTargetCatalogGateway>,
    current_executable: PathBuf,
}

impl RemoteMainSlotPaneRuntime {
    pub fn from_build_env() -> Result<Self, LifecycleError> {
        let current_executable = std::env::current_exe().map_err(|error| {
            LifecycleError::Io(
                "failed to locate current waitagent executable".to_string(),
                error,
            )
        })?;
        Ok(Self {
            target_registry: TargetRegistryService::new(
                DefaultTargetCatalogGateway::from_build_env().map_err(remote_pane_error)?,
            ),
            current_executable,
        })
    }

    pub fn run(&self, command: RemoteMainSlotCommand) -> Result<(), LifecycleError> {
        let target = self.resolve_remote_target(&command)?;
        let terminal = TerminalRuntime::stdio();
        let initial_size = terminal.current_size_or_default();
        let _raw_mode = terminal.enter_raw_mode()?;
        let _cursor_guard = RemotePaneCursorGuard::hide().map_err(|error| {
            LifecycleError::Io("failed to hide remote main-slot cursor".to_string(), error)
        })?;

        let registry = RemoteConnectionRegistry::new();
        let remote_runtime = RemoteMainSlotRuntime::with_registry(registry.clone());
        let mailbox = remote_runtime
            .ensure_local_observer_connection(command.socket_name.clone())
            .ok_or_else(|| {
                LifecycleError::Protocol(
                    "remote observer connection registry is not available".to_string(),
                )
            })?;
        let binding = remote_runtime.activate_target(
            &target,
            RemoteConsoleDescriptor {
                console_id: remote_console_id(&command),
                console_host_id: command.socket_name.clone(),
                location: ConsoleLocation::LocalWorkspace,
            },
            usize::from(initial_size.cols),
            usize::from(initial_size.rows),
        )?;
        let mut observer = RemoteObserverRuntime::new(
            mailbox.clone(),
            usize::from(initial_size.cols),
            usize::from(initial_size.rows),
        );
        observer.sync().map_err(remote_protocol_error)?;

        let (event_tx, event_rx) = mpsc::channel();
        spawn_input_thread(event_tx.clone());
        let resize_watcher = spawn_resize_watcher(event_tx.clone()).map_err(remote_pane_error)?;
        spawn_mailbox_watcher(mailbox, event_tx.clone());
        let authority_transport_socket_path = authority_transport_socket_path(
            &command.socket_name,
            &command.session_name,
            &command.target,
        );
        let _authority_listener = spawn_authority_listener(
            authority_transport_socket_path.clone(),
            registry.clone(),
            &target,
            event_tx,
        )
        .map_err(remote_pane_error)?;
        self.spawn_local_authority_target_host_if_resolvable(
            &target,
            &authority_transport_socket_path,
        )
        .map_err(remote_pane_error)?;
        thread::spawn(move || {
            let _keep_resize_watcher_alive = resize_watcher;
            thread::park();
        });
        let mut console_seq = 0u64;
        draw_remote_snapshot(
            &terminal,
            &target,
            &binding,
            &observer.snapshot(),
            remote_runtime.has_connection(target.address.authority_id()),
        )?;

        loop {
            match event_rx.recv() {
                Ok(RemotePaneEvent::MailboxUpdated) => {
                    observer.sync().map_err(remote_protocol_error)?;
                    draw_remote_snapshot(
                        &terminal,
                        &target,
                        &binding,
                        &observer.snapshot(),
                        remote_runtime.has_connection(target.address.authority_id()),
                    )?;
                }
                Ok(RemotePaneEvent::Resize) => {
                    draw_remote_snapshot(
                        &terminal,
                        &target,
                        &binding,
                        &observer.snapshot(),
                        remote_runtime.has_connection(target.address.authority_id()),
                    )?;
                }
                Ok(RemotePaneEvent::AuthorityTransportChanged) => {
                    draw_remote_snapshot(
                        &terminal,
                        &target,
                        &binding,
                        &observer.snapshot(),
                        remote_runtime.has_connection(target.address.authority_id()),
                    )?;
                }
                Ok(RemotePaneEvent::AuthorityEnvelope(envelope)) => {
                    apply_authority_envelope(&remote_runtime, &target, &envelope)
                        .map_err(remote_protocol_error)?;
                }
                Ok(RemotePaneEvent::Input(bytes)) => {
                    if remote_runtime.has_connection(target.address.authority_id()) {
                        console_seq += 1;
                        remote_runtime.send_console_input(
                            &target,
                            &binding,
                            console_seq,
                            encode_base64(&bytes),
                        )?;
                    }
                }
                Err(_) => return Ok(()),
            }
        }
    }

    fn resolve_remote_target(
        &self,
        command: &RemoteMainSlotCommand,
    ) -> Result<ManagedSessionRecord, LifecycleError> {
        let session = self
            .target_registry
            .find_target(&command.target)
            .map_err(remote_pane_error)?
            .ok_or_else(|| {
                LifecycleError::Protocol(format!(
                    "unknown remote target `{}` for remote main-slot pane",
                    command.target
                ))
            })?;
        if session.address.transport() != &SessionTransport::RemotePeer {
            return Err(LifecycleError::Protocol(format!(
                "target `{}` is not a remote target",
                command.target
            )));
        }
        Ok(session)
    }

    fn spawn_local_authority_target_host_if_resolvable(
        &self,
        target: &ManagedSessionRecord,
        transport_socket_path: &Path,
    ) -> Result<(), RemoteSocketTransportError> {
        let available_targets = self
            .target_registry
            .list_targets()
            .map_err(|error| RemoteSocketTransportError::new(error.to_string()))?;
        let Some(resolved) = resolve_local_authority_target_host(target, &available_targets) else {
            return Ok(());
        };
        let mut command = Command::new(&self.current_executable);
        command
            .args(remote_authority_target_host_args(
                &resolved,
                target,
                transport_socket_path,
            ))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command.spawn()?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedLocalAuthorityTargetHost {
    socket_name: String,
    target_session_name: String,
}

#[derive(Debug)]
enum RemotePaneEvent {
    Input(Vec<u8>),
    Resize,
    MailboxUpdated,
    AuthorityTransportChanged,
    AuthorityEnvelope(ProtocolEnvelope<ControlPlanePayload>),
}

struct RemotePaneResizeWatcher {
    _writer: UnixStream,
}

struct RemotePaneCursorGuard {
    visible_on_drop: bool,
}

struct AuthorityListenerGuard {
    socket_path: PathBuf,
}

struct SocketRemoteControlPlaneConnection {
    writer: Arc<Mutex<UnixStream>>,
    connected: Arc<AtomicBool>,
}

impl RemotePaneCursorGuard {
    fn hide() -> io::Result<Self> {
        write_escape(HIDE_CURSOR_ESCAPE)?;
        Ok(Self {
            visible_on_drop: true,
        })
    }
}

impl Drop for RemotePaneCursorGuard {
    fn drop(&mut self) {
        if self.visible_on_drop {
            let _ = write_escape(SHOW_CURSOR_ESCAPE);
        }
    }
}

impl Drop for RemotePaneResizeWatcher {
    fn drop(&mut self) {
        REMOTE_PANE_SIGWINCH_WRITE_FD.store(-1, Ordering::Relaxed);
    }
}

impl Drop for AuthorityListenerGuard {
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

fn spawn_input_thread(tx: mpsc::Sender<RemotePaneEvent>) {
    thread::spawn(move || {
        let mut stdin = io::stdin().lock();
        let mut buffer = [0u8; 64];
        loop {
            match stdin.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    if tx
                        .send(RemotePaneEvent::Input(buffer[..read].to_vec()))
                        .is_err()
                    {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
}

fn spawn_resize_watcher(tx: mpsc::Sender<RemotePaneEvent>) -> io::Result<RemotePaneResizeWatcher> {
    let (mut reader, writer) = UnixStream::pair()?;
    REMOTE_PANE_SIGWINCH_WRITE_FD.store(writer.as_raw_fd(), Ordering::Relaxed);
    unsafe {
        signal(SIGWINCH, remote_pane_sigwinch_handler);
    }

    thread::spawn(move || {
        let mut buffer = [0_u8; 64];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(_) => {
                    if tx.send(RemotePaneEvent::Resize).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    Ok(RemotePaneResizeWatcher { _writer: writer })
}

fn spawn_mailbox_watcher(mailbox: LocalNodeMailbox, tx: mpsc::Sender<RemotePaneEvent>) {
    thread::spawn(move || {
        let mut seen = mailbox.snapshot().len();
        loop {
            mailbox.wait_for_growth(seen);
            let current = mailbox.snapshot().len();
            if current <= seen {
                continue;
            }
            seen = current;
            if tx.send(RemotePaneEvent::MailboxUpdated).is_err() {
                break;
            }
        }
    });
}

fn spawn_authority_listener(
    socket_path: PathBuf,
    registry: RemoteConnectionRegistry,
    target: &ManagedSessionRecord,
    tx: mpsc::Sender<RemotePaneEvent>,
) -> io::Result<AuthorityListenerGuard> {
    if socket_path.exists() {
        let _ = fs::remove_file(&socket_path);
    }
    let listener = UnixListener::bind(&socket_path)?;
    let authority_id = target.address.authority_id().to_string();

    thread::spawn(move || {
        for accepted in listener.incoming() {
            let Ok(stream) = accepted else {
                break;
            };
            let _ = register_authority_stream(
                stream,
                registry.clone(),
                authority_id.clone(),
                tx.clone(),
            );
        }
    });

    Ok(AuthorityListenerGuard { socket_path })
}

fn register_authority_stream(
    mut stream: UnixStream,
    registry: RemoteConnectionRegistry,
    authority_id: String,
    tx: mpsc::Sender<RemotePaneEvent>,
) -> Result<(), RemoteSocketTransportError> {
    let node_id = read_registration_frame(&mut stream)?;
    if node_id != authority_id {
        return Err(RemoteSocketTransportError::new(format!(
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
                        .send(RemotePaneEvent::AuthorityEnvelope(envelope))
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
        let _ = reader_tx.send(RemotePaneEvent::AuthorityTransportChanged);
    });

    let _ = tx.send(RemotePaneEvent::AuthorityTransportChanged);
    Ok(())
}

fn apply_authority_envelope(
    remote_runtime: &RemoteMainSlotRuntime,
    target: &ManagedSessionRecord,
    envelope: &ProtocolEnvelope<ControlPlanePayload>,
) -> Result<(), RemoteSocketTransportError> {
    match &envelope.payload {
        ControlPlanePayload::TargetOutput(payload) => {
            if envelope.sender_id != target.address.authority_id() {
                return Err(RemoteSocketTransportError::new(format!(
                    "authority envelope sender `{}` does not match target authority `{}`",
                    envelope.sender_id,
                    target.address.authority_id()
                )));
            }
            remote_runtime
                .send_target_output(
                    target,
                    payload.output_seq,
                    payload.stream,
                    payload.bytes_base64.clone(),
                )
                .map_err(|error| RemoteSocketTransportError::new(error.to_string()))
        }
        other => Err(RemoteSocketTransportError::new(format!(
            "unexpected authority envelope payload `{}`",
            other.message_type()
        ))),
    }
}

fn draw_remote_snapshot(
    terminal: &TerminalRuntime,
    target: &ManagedSessionRecord,
    binding: &RemoteAttachmentBinding,
    snapshot: &RemoteObserverSnapshot,
    authority_connected: bool,
) -> Result<(), LifecycleError> {
    let viewport = terminal.current_size_or_default();
    let screen_lines = if snapshot.last_output_seq.is_some() {
        snapshot.active_screen().styled_lines.clone()
    } else {
        placeholder_lines(target, binding, authority_connected, viewport)
    };
    let active_screen = snapshot.active_screen();

    let mut stdout = io::stdout().lock();
    for row in 0..usize::from(viewport.rows.max(1)) {
        let line = screen_lines.get(row).map(String::as_str).unwrap_or("");
        write!(stdout, "\x1b[{};1H{}\x1b[K", row + 1, line).map_err(|error| {
            LifecycleError::Io("failed to draw remote main-slot output".to_string(), error)
        })?;
    }

    if snapshot.last_output_seq.is_some() && active_screen.cursor_visible {
        write!(
            stdout,
            "\x1b[{};{}H\x1b[?25h",
            usize::from(active_screen.cursor_row.saturating_add(1)),
            usize::from(active_screen.cursor_col.saturating_add(1))
        )
        .map_err(|error| {
            LifecycleError::Io(
                "failed to position remote main-slot cursor".to_string(),
                error,
            )
        })?;
    } else {
        write!(stdout, "\x1b[?25l").map_err(|error| {
            LifecycleError::Io("failed to hide remote main-slot cursor".to_string(), error)
        })?;
    }
    stdout.flush().map_err(|error| {
        LifecycleError::Io("failed to flush remote main-slot output".to_string(), error)
    })
}

fn placeholder_lines(
    target: &ManagedSessionRecord,
    binding: &RemoteAttachmentBinding,
    authority_connected: bool,
    viewport: TerminalSize,
) -> Vec<String> {
    let mut lines = vec![
        format!(
            "remote target {}",
            target
                .command_name
                .as_deref()
                .unwrap_or(target.address.session_id())
        ),
        format!("target-id: {}", target.address.id().as_str()),
        format!("attachment: {}", binding.attachment_id),
        format!(
            "authority transport: {}",
            if authority_connected {
                "connected"
            } else {
                "waiting"
            }
        ),
    ];
    if !authority_connected {
        lines.push(
            "input and PTY resize stay local until a live authority connection is registered"
                .to_string(),
        );
    }

    while lines.len() < usize::from(viewport.rows.max(1)) {
        lines.push(String::new());
    }
    lines
}

fn resolve_local_authority_target_host(
    target: &ManagedSessionRecord,
    available_targets: &[ManagedSessionRecord],
) -> Option<ResolvedLocalAuthorityTargetHost> {
    let selector = target.selector.as_deref()?;
    let local_target_host = available_targets.iter().find(|candidate| {
        candidate.address.transport() == &SessionTransport::LocalTmux
            && candidate.is_target_host()
            && candidate.matches_target(selector)
    })?;
    Some(ResolvedLocalAuthorityTargetHost {
        socket_name: local_target_host.address.server_id().to_string(),
        target_session_name: local_target_host.address.session_id().to_string(),
    })
}

fn remote_authority_target_host_args(
    resolved: &ResolvedLocalAuthorityTargetHost,
    target: &ManagedSessionRecord,
    transport_socket_path: &Path,
) -> Vec<String> {
    vec![
        "__remote-authority-target-host".to_string(),
        "--socket-name".to_string(),
        resolved.socket_name.clone(),
        "--target-session-name".to_string(),
        resolved.target_session_name.clone(),
        "--authority-id".to_string(),
        target.address.authority_id().to_string(),
        "--target-id".to_string(),
        target.address.id().as_str().to_string(),
        "--transport-socket-path".to_string(),
        transport_socket_path.display().to_string(),
    ]
}

fn remote_console_id(command: &RemoteMainSlotCommand) -> String {
    format!(
        "workspace-main-slot:{}:{}",
        command.socket_name, command.session_name
    )
}

fn write_escape(sequence: &str) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    stdout.write_all(sequence.as_bytes())?;
    stdout.flush()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RemoteSocketTransportError {
    message: String,
}

impl RemoteSocketTransportError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for RemoteSocketTransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for RemoteSocketTransportError {}

impl From<io::Error> for RemoteSocketTransportError {
    fn from(value: io::Error) -> Self {
        Self::new(value.to_string())
    }
}

impl From<RemoteTransportCodecError> for RemoteSocketTransportError {
    fn from(value: RemoteTransportCodecError) -> Self {
        Self::new(value.to_string())
    }
}

fn remote_protocol_error(error: impl ToString) -> LifecycleError {
    LifecycleError::Protocol(error.to_string())
}

fn remote_pane_error<E>(error: E) -> LifecycleError
where
    E: ToString,
{
    LifecycleError::Io(
        "failed to run remote main-slot pane".to_string(),
        io::Error::new(io::ErrorKind::Other, error.to_string()),
    )
}

extern "C" fn remote_pane_sigwinch_handler(_signal: c_int) {
    let fd = REMOTE_PANE_SIGWINCH_WRITE_FD.load(Ordering::Relaxed);
    if fd < 0 {
        return;
    }

    let byte = 1_u8;
    unsafe {
        let _ = write(fd, (&byte as *const u8).cast::<c_void>(), 1);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        apply_authority_envelope, encode_base64, placeholder_lines, register_authority_stream,
        remote_authority_target_host_args, remote_console_id, resolve_local_authority_target_host,
        RemotePaneEvent, ResolvedLocalAuthorityTargetHost,
    };
    use crate::cli::RemoteMainSlotCommand;
    use crate::domain::session_catalog::{
        ConsoleLocation, ManagedSessionAddress, ManagedSessionRecord, ManagedSessionTaskState,
        SessionAvailability,
    };
    use crate::domain::workspace::WorkspaceSessionRole;
    use crate::infra::remote_protocol::{
        ControlPlanePayload, ProtocolEnvelope, RemoteConsoleDescriptor, TargetOutputPayload,
    };
    use crate::infra::remote_transport_codec::{
        write_control_plane_envelope, write_registration_frame,
    };
    use crate::runtime::remote_authority_transport_runtime::{
        authority_transport_socket_path, RemoteAuthorityCommand, RemoteAuthorityTransportRuntime,
    };
    use crate::runtime::remote_main_slot_runtime::RemoteAttachmentBinding;
    use crate::runtime::remote_main_slot_runtime::RemoteMainSlotRuntime;
    use crate::runtime::remote_observer_runtime::RemoteObserverRuntime;
    use crate::runtime::remote_transport_runtime::RemoteConnectionRegistry;
    use crate::terminal::TerminalSize;
    use std::fs;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::Path;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn remote_console_id_matches_workspace_main_slot_shape() {
        let command = RemoteMainSlotCommand {
            socket_name: "wa-1".to_string(),
            session_name: "workspace-1".to_string(),
            target: "peer-a:shell-1".to_string(),
        };

        assert_eq!(
            remote_console_id(&command),
            "workspace-main-slot:wa-1:workspace-1"
        );
    }

    #[test]
    fn encode_base64_matches_standard_output_for_short_chunks() {
        assert_eq!(encode_base64(b"a"), "YQ==");
        assert_eq!(encode_base64(b"ab"), "YWI=");
        assert_eq!(encode_base64(b"abc"), "YWJj");
    }

    #[test]
    fn placeholder_lines_explain_transport_gap_before_output_arrives() {
        let lines = placeholder_lines(
            &remote_target(),
            &RemoteAttachmentBinding {
                target_id: "remote-peer:peer-a:shell-1".to_string(),
                attachment_id: "attach-1".to_string(),
                console_id: "console-a".to_string(),
            },
            false,
            TerminalSize {
                rows: 5,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            },
        );

        assert_eq!(lines.len(), 5);
        assert!(lines[0].contains("remote target bash"));
        assert!(lines[3].contains("waiting"));
        assert!(lines[4].contains("input and PTY resize stay local"));
    }

    #[test]
    fn authority_transport_socket_path_is_workspace_and_target_scoped() {
        let command = RemoteMainSlotCommand {
            socket_name: "wa-1".to_string(),
            session_name: "workspace-1".to_string(),
            target: "peer-a:shell-1".to_string(),
        };

        let path = authority_transport_socket_path(
            &command.socket_name,
            &command.session_name,
            &command.target,
        );
        let rendered = path.to_string_lossy();

        assert!(rendered.contains("waitagent-remote-wa-1-workspace-1-peer-a_shell-1.sock"));
    }

    #[test]
    fn register_authority_stream_tracks_connection_and_forwards_inbound_envelopes() {
        let registry = RemoteConnectionRegistry::new();
        let (tx, rx) = mpsc::channel();
        let (mut client, server) = UnixStream::pair().expect("stream pair should open");

        write_registration_frame(&mut client, "peer-a").expect("registration frame should encode");
        register_authority_stream(server, registry.clone(), "peer-a".to_string(), tx)
            .expect("authority stream should register");

        assert!(registry.has_connection("peer-a"));
        assert!(matches!(
            rx.recv().expect("transport change should be emitted"),
            RemotePaneEvent::AuthorityTransportChanged
        ));

        write_control_plane_envelope(&mut client, &authority_target_output_envelope(1))
            .expect("target output should encode");
        match rx.recv().expect("authority envelope should arrive") {
            RemotePaneEvent::AuthorityEnvelope(envelope) => {
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
    fn authority_target_output_envelope_flows_back_into_observer_terminal_state() {
        let runtime = RemoteMainSlotRuntime::with_registry(RemoteConnectionRegistry::new());
        let mailbox = runtime
            .ensure_local_observer_connection("observer-a")
            .expect("observer loopback registration should succeed");
        runtime.ensure_local_connection("peer-a");
        let target = remote_target();

        runtime
            .activate_target(
                &target,
                crate::infra::remote_protocol::RemoteConsoleDescriptor {
                    console_id: "console-a".to_string(),
                    console_host_id: "observer-a".to_string(),
                    location: crate::domain::session_catalog::ConsoleLocation::LocalWorkspace,
                },
                12,
                4,
            )
            .expect("remote activation should succeed");

        apply_authority_envelope(&runtime, &target, &authority_target_output_envelope(1))
            .expect("authority target_output should apply");

        let mut observer = RemoteObserverRuntime::new(mailbox, 12, 4);
        observer.sync().expect("observer sync should succeed");
        let snapshot = observer.snapshot();
        assert_eq!(snapshot.last_output_seq, Some(1));
        assert_eq!(
            snapshot.active_screen().lines[0],
            "a           ".to_string()
        );
    }

    #[test]
    fn authority_transport_runtime_round_trips_resize_input_and_output() {
        let registry = RemoteConnectionRegistry::new();
        let runtime = RemoteMainSlotRuntime::with_registry(registry.clone());
        let mailbox = runtime
            .ensure_local_observer_connection("observer-a")
            .expect("observer loopback registration should succeed");
        let target = remote_target();
        let binding = runtime
            .activate_target(
                &target,
                RemoteConsoleDescriptor {
                    console_id: "console-a".to_string(),
                    console_host_id: "observer-a".to_string(),
                    location: ConsoleLocation::LocalWorkspace,
                },
                12,
                4,
            )
            .expect("remote activation should succeed");
        let socket_path = authority_transport_socket_path("wa-1", "workspace-1", "peer-a:shell-1");
        let _ = fs::remove_file(&socket_path);
        let listener = UnixListener::bind(&socket_path).expect("listener should bind");
        let (tx, rx) = mpsc::channel();
        let accept_registry = registry.clone();

        let accept_thread = thread::spawn(move || {
            let (stream, _) = listener.accept().expect("listener should accept");
            register_authority_stream(stream, accept_registry, "peer-a".to_string(), tx)
                .expect("authority stream should register");
        });

        let authority = RemoteAuthorityTransportRuntime::connect(&socket_path, "peer-a")
            .expect("authority runtime should connect");
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(1))
                .expect("transport change should arrive"),
            RemotePaneEvent::AuthorityTransportChanged
        ));

        runtime
            .send_pty_resize(&target, &binding, 160, 50)
            .expect("resize should route");
        assert_eq!(
            authority
                .recv_command()
                .expect("resize command should arrive"),
            RemoteAuthorityCommand::ApplyResize(
                crate::infra::remote_protocol::ApplyResizePayload {
                    target_id: "remote-peer:peer-a:shell-1".to_string(),
                    resize_epoch: 1,
                    resize_authority_console_id: "console-a".to_string(),
                    cols: 160,
                    rows: 50,
                }
            )
        );

        runtime
            .send_console_input(&target, &binding, 1, "YQ==")
            .expect("input should route");
        assert_eq!(
            authority
                .recv_command()
                .expect("input command should arrive"),
            RemoteAuthorityCommand::TargetInput(
                crate::infra::remote_protocol::TargetInputPayload {
                    attachment_id: "attach-1".to_string(),
                    target_id: "remote-peer:peer-a:shell-1".to_string(),
                    console_id: "console-a".to_string(),
                    console_host_id: "observer-a".to_string(),
                    input_seq: 1,
                    bytes_base64: "YQ==".to_string(),
                }
            )
        );

        authority
            .send_target_output("remote-peer:peer-a:shell-1", 1, "pty", "Yg==")
            .expect("target output should send");
        match rx
            .recv_timeout(Duration::from_secs(1))
            .expect("authority envelope should arrive")
        {
            RemotePaneEvent::AuthorityEnvelope(envelope) => {
                apply_authority_envelope(&runtime, &target, &envelope)
                    .expect("authority output should apply");
            }
            other => panic!("unexpected event: {other:?}"),
        }

        let mut observer = RemoteObserverRuntime::new(mailbox, 12, 4);
        observer.sync().expect("observer sync should succeed");
        let snapshot = observer.snapshot();
        assert_eq!(snapshot.last_output_seq, Some(1));
        assert_eq!(
            snapshot.active_screen().lines[0],
            "b           ".to_string()
        );

        accept_thread
            .join()
            .expect("accept thread should join cleanly");
        let _ = fs::remove_file(&socket_path);
    }

    #[test]
    fn resolve_local_authority_target_host_uses_selector_to_find_local_target_host() {
        let resolved = resolve_local_authority_target_host(
            &remote_target_with_selector("wa-local:shell-host"),
            &[local_target_host("wa-local", "shell-host")],
        )
        .expect("selector should resolve local authority target host");

        assert_eq!(
            resolved,
            ResolvedLocalAuthorityTargetHost {
                socket_name: "wa-local".to_string(),
                target_session_name: "shell-host".to_string(),
            }
        );
    }

    #[test]
    fn resolve_local_authority_target_host_ignores_missing_or_non_host_selector_targets() {
        assert!(resolve_local_authority_target_host(
            &remote_target(),
            &[local_target_host("wa-1", "shell-1")]
        )
        .is_none());
        assert!(resolve_local_authority_target_host(
            &remote_target_with_selector("wa-local:workspace"),
            &[local_workspace_chrome("wa-local", "workspace")],
        )
        .is_none());
        assert!(resolve_local_authority_target_host(
            &remote_target_with_selector("peer-a:shell-1"),
            &[remote_target_with_selector("wa-local:shell-host")],
        )
        .is_none());
    }

    #[test]
    fn remote_authority_target_host_args_bind_local_target_host_to_remote_target_id() {
        let args = remote_authority_target_host_args(
            &ResolvedLocalAuthorityTargetHost {
                socket_name: "wa-local".to_string(),
                target_session_name: "shell-host".to_string(),
            },
            &remote_target_with_selector("wa-local:shell-host"),
            Path::new("/tmp/authority.sock"),
        );

        assert_eq!(
            args,
            vec![
                "__remote-authority-target-host",
                "--socket-name",
                "wa-local",
                "--target-session-name",
                "shell-host",
                "--authority-id",
                "peer-a",
                "--target-id",
                "remote-peer:peer-a:shell-1",
                "--transport-socket-path",
                "/tmp/authority.sock",
            ]
        );
    }

    fn authority_target_output_envelope(output_seq: u64) -> ProtocolEnvelope<ControlPlanePayload> {
        ProtocolEnvelope {
            protocol_version: "1.1".to_string(),
            message_id: format!("msg-{output_seq}"),
            message_type: "target_output",
            timestamp: "2026-04-28T00:00:00Z".to_string(),
            sender_id: "peer-a".to_string(),
            correlation_id: None,
            target_id: Some("remote-peer:peer-a:shell-1".to_string()),
            attachment_id: None,
            console_id: None,
            payload: ControlPlanePayload::TargetOutput(TargetOutputPayload {
                target_id: "remote-peer:peer-a:shell-1".to_string(),
                output_seq,
                stream: "pty",
                bytes_base64: "YQ==".to_string(),
            }),
        }
    }

    fn remote_target() -> ManagedSessionRecord {
        ManagedSessionRecord {
            address: ManagedSessionAddress::remote_peer("peer-a", "shell-1"),
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

    fn remote_target_with_selector(selector: &str) -> ManagedSessionRecord {
        let mut target = remote_target();
        target.selector = Some(selector.to_string());
        target
    }

    fn local_target_host(socket_name: &str, session_name: &str) -> ManagedSessionRecord {
        ManagedSessionRecord {
            address: ManagedSessionAddress::local_tmux(socket_name, session_name),
            selector: Some(format!("{socket_name}:{session_name}")),
            availability: SessionAvailability::Online,
            workspace_dir: None,
            workspace_key: None,
            session_role: Some(WorkspaceSessionRole::TargetHost),
            opened_by: Vec::new(),
            attached_clients: 0,
            window_count: 1,
            command_name: Some("bash".to_string()),
            current_path: None,
            task_state: ManagedSessionTaskState::Running,
        }
    }

    fn local_workspace_chrome(socket_name: &str, session_name: &str) -> ManagedSessionRecord {
        ManagedSessionRecord {
            address: ManagedSessionAddress::local_tmux(socket_name, session_name),
            selector: Some(format!("{socket_name}:{session_name}")),
            availability: SessionAvailability::Online,
            workspace_dir: None,
            workspace_key: None,
            session_role: Some(WorkspaceSessionRole::WorkspaceChrome),
            opened_by: Vec::new(),
            attached_clients: 1,
            window_count: 1,
            command_name: Some("bash".to_string()),
            current_path: None,
            task_state: ManagedSessionTaskState::Input,
        }
    }
}
