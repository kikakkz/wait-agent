use crate::application::target_registry_service::{
    DefaultTargetCatalogGateway, TargetRegistryService,
};
use crate::cli::RemoteMainSlotCommand;
use crate::domain::session_catalog::{ConsoleLocation, ManagedSessionRecord, SessionAvailability};
use crate::infra::error_log::ERROR_LOG;
use crate::infra::remote_protocol::{
    ControlPlanePayload, ProtocolEnvelope, RawPtyInputPayload, RawPtyOutputPayload,
    RemoteConsoleDescriptor,
};
use crate::infra::remote_transport_codec::RemoteTransportCodecError;
use crate::infra::tmux::{EmbeddedTmuxBackend, TmuxError};
use crate::lifecycle::LifecycleError;
use crate::runtime::remote_authority_connection_runtime::AuthorityTransportEvent;
use crate::runtime::remote_main_slot_runtime::{RemoteAttachmentBinding, RemoteMainSlotRuntime};
use crate::runtime::remote_observer_runtime::{RemoteObserverRuntime, RemoteObserverSnapshot};
use crate::runtime::remote_transport_runtime::{LocalNodeMailbox, RemoteConnectionRegistry};
use crate::terminal::{ScreenSnapshot, TerminalRuntime, TerminalSize};
use std::cell::RefCell;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::raw::{c_int, c_void};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use super::{AuthorityTransportStatus, RemoteInteractSignal, RemoteInteractSurfaceSpec};

const SIGWINCH: c_int = 28;
const HIDE_CURSOR_ESCAPE: &str = "\x1b[?25l";
const SHOW_CURSOR_ESCAPE: &str = "\x1b[?25h";
pub(super) const CLEAR_SCREEN_HOME_ESCAPE: &str = "\x1b[2J\x1b[H";
const TARGET_PRESENCE_MISS_GRACE_POLLS: usize = 4;
pub(crate) const RECONNECT_ANIMATION_INTERVAL: Duration = Duration::from_millis(200);
pub(crate) const INITIAL_CONNECT_TIMEOUT: Duration = Duration::from_secs(120);
pub(crate) const RECONNECT_TIMEOUT: Duration = Duration::from_secs(60);
const RECONNECT_SPINNER_FRAMES: &[char] = &['⠋', '⠙', '⠸', '⠴', '⠦', '⠧', '⠇', '⠏'];

static REMOTE_PANE_SIGWINCH_WRITE_FD: AtomicI32 = AtomicI32::new(-1);
static REMOTE_DRAW_DEBUG_SEQ: AtomicU64 = AtomicU64::new(0);

// Thread-local override for session output. When set, remote session
// content is written to this file (a session-specific tmux pane TTY)
// instead of stdout. This gives each session its own scrollback buffer.
thread_local! {
    static SESSION_OUTPUT: RefCell<Option<File>> = const { RefCell::new(None) };
}

fn with_session_output<R>(f: impl FnOnce(&mut File) -> R) -> Option<R> {
    SESSION_OUTPUT.with(|o| o.borrow_mut().as_mut().map(|file| f(file)))
}

/// Return a `Box<dyn Write>` pointing to the session pane TTY when
/// session output isolation is active, or `io::stdout()` otherwise.
fn session_stdout() -> Box<dyn Write + 'static> {
    match with_session_output(|f| f.try_clone()) {
        Some(Ok(file)) => Box::new(file),
        _ => Box::new(io::stdout()),
    }
}

extern "C" {
    fn signal(signum: c_int, handler: extern "C" fn(c_int)) -> usize;
    fn write(fd: c_int, buf: *const c_void, count: usize) -> isize;
}

#[cfg(test)]
pub(super) fn activate_surface_target(
    remote_runtime: &RemoteMainSlotRuntime,
    target: &ManagedSessionRecord,
    spec: &RemoteInteractSurfaceSpec,
    size: &TerminalSize,
    observer: &mut RemoteObserverRuntime,
) -> Result<RemoteAttachmentBinding, LifecycleError> {
    activate_surface_target_with_mode(remote_runtime, target, spec, size, observer)
        .map(|(binding, _)| binding)
}

pub(super) fn activate_surface_target_with_mode(
    remote_runtime: &RemoteMainSlotRuntime,
    target: &ManagedSessionRecord,
    spec: &RemoteInteractSurfaceSpec,
    size: &TerminalSize,
    observer: &mut RemoteObserverRuntime,
) -> Result<(RemoteAttachmentBinding, Vec<u8>), LifecycleError> {
    let had_visible_output = observer.snapshot().has_visible_output;
    if had_visible_output {
        // Reconnect path: keep the last known screen visible while the
        // new mirror is being set up. Only clear sequence tracking so
        // incoming TargetOutput frames won't be rejected as out-of-order.
        // The first BootstrapChunk or TargetOutput will overwrite the old
        // terminal state naturally via feed().
        observer.clear_output_seq();
    } else {
        observer.begin_bootstrap();
    }
    let binding = remote_runtime.activate_target_with_raw_pty_mode(
        target,
        RemoteConsoleDescriptor {
            console_id: spec.console_id.clone(),
            console_host_id: spec.console_host_id.clone(),
            location: spec.console_location,
        },
        usize::from(size.cols),
        usize::from(size.rows),
        true,
    )?;
    let raw = observer
        .sync_and_collect_raw()
        .map_err(remote_protocol_error)?;
    Ok((binding, raw))
}

pub(super) fn should_draw_remote_snapshot(
    binding: Option<&RemoteAttachmentBinding>,
    snapshot: &RemoteObserverSnapshot,
    authority_status: &AuthorityTransportStatus,
) -> bool {
    // Draw placeholder when there's no active raw PTY binding,
    // or when we're still waiting for initial authority data.
    binding.is_none()
        || (!snapshot.has_visible_output
            && matches!(
                authority_status,
                AuthorityTransportStatus::WaitingForRemoteAuthority
            ))
}

pub(super) fn write_remote_raw_output(bytes: &[u8]) -> Result<(), LifecycleError> {
    if bytes.is_empty() {
        return Ok(());
    }
    // Route through the per-session TTY when session output isolation is active.
    if with_session_output(|f| f.write_all(bytes).and_then(|_| f.flush())).is_some() {
        return Ok(());
    }
    let mut stdout = io::stdout().lock();
    stdout.write_all(bytes).map_err(|error| {
        LifecycleError::Io("failed to write remote raw output".to_string(), error)
    })?;
    stdout
        .flush()
        .map_err(|error| LifecycleError::Io("failed to flush remote raw output".to_string(), error))
}

pub(super) fn write_remote_raw_output_with_initial_clear(
    bytes: &[u8],
    screen_initialized: &mut bool,
) -> Result<(), LifecycleError> {
    if bytes.is_empty() {
        return Ok(());
    }
    if !*screen_initialized {
        let clear = CLEAR_SCREEN_HOME_ESCAPE.as_bytes();
        if with_session_output(|f| f.write_all(clear).and_then(|_| f.flush())).is_none() {
            write_escape(CLEAR_SCREEN_HOME_ESCAPE).map_err(remote_pane_error)?;
        }
        *screen_initialized = true;
    }
    write_remote_raw_output(bytes)
}

pub(super) fn collect_direct_raw_pty_output_envelope(
    target: &ManagedSessionRecord,
    envelope: &ProtocolEnvelope<ControlPlanePayload>,
    last_output_seq: &mut Option<u64>,
) -> Result<Option<Vec<u8>>, RemoteSocketTransportError> {
    let ControlPlanePayload::RawPtyOutput(payload) = &envelope.payload else {
        return Ok(None);
    };
    if envelope.sender_id != target.address.authority_id() {
        return Err(RemoteSocketTransportError::new(format!(
            "authority envelope sender `{}` does not match target authority `{}`",
            envelope.sender_id,
            target.address.authority_id()
        )));
    }
    if !output_payload_matches_target(
        payload.session_id.as_str(),
        payload.target_id.as_str(),
        target,
    ) {
        ERROR_LOG.log(format!(
            "dropping raw PTY output for wrong target: expected {}:{}, got {}:{}",
            target.address.session_id(),
            target.address.id().as_str(),
            payload.session_id,
            payload.target_id
        ));
        return Ok(None);
    }
    if let Some(last) = *last_output_seq {
        if payload.output_seq <= last {
            return Err(RemoteSocketTransportError::new(format!(
                "remote raw PTY received out-of-order output for `{}`: {} after {}",
                payload.target_id, payload.output_seq, last
            )));
        }
    }
    *last_output_seq = Some(payload.output_seq);
    Ok(Some(payload.output_bytes.clone()))
}

pub(super) fn collect_direct_raw_pty_output_payload(
    target: &ManagedSessionRecord,
    authority_id: &str,
    payload: &RawPtyOutputPayload,
    last_output_seq: &mut Option<u64>,
) -> Result<Vec<u8>, RemoteSocketTransportError> {
    if authority_id != target.address.authority_id() {
        return Err(RemoteSocketTransportError::new(format!(
            "authority `{}` does not match target authority `{}`",
            authority_id,
            target.address.authority_id()
        )));
    }
    if !output_payload_matches_target(
        payload.session_id.as_str(),
        payload.target_id.as_str(),
        target,
    ) {
        ERROR_LOG.log(format!(
            "dropping raw PTY output for wrong target: expected {}:{}, got {}:{}",
            target.address.session_id(),
            target.address.id().as_str(),
            payload.session_id,
            payload.target_id
        ));
        return Ok(Vec::new());
    }
    if let Some(last) = *last_output_seq {
        if payload.output_seq <= last {
            return Err(RemoteSocketTransportError::new(format!(
                "remote raw PTY received out-of-order output for `{}`: {} after {}",
                payload.target_id, payload.output_seq, last
            )));
        }
    }
    *last_output_seq = Some(payload.output_seq);
    Ok(payload.output_bytes.clone())
}

fn output_payload_matches_target(
    payload_session_id: &str,
    payload_target_id: &str,
    target: &ManagedSessionRecord,
) -> bool {
    payload_session_id == target.address.session_id()
        && payload_target_id == target.address.id().as_str()
}

pub(super) fn should_exit_surface_locally(spec: &RemoteInteractSurfaceSpec, bytes: &[u8]) -> bool {
    spec.console_location == ConsoleLocation::ServerConsole && bytes.contains(&0x1d)
}

pub(super) fn should_exit_surface_for_target_presence_loss(
    target_availability: Option<SessionAvailability>,
    authority_connected: bool,
    reconnecting: bool,
) -> bool {
    if target_availability.is_none() {
        return true;
    }
    if target_availability == Some(SessionAvailability::Exited) {
        return true;
    }
    if target_availability == Some(SessionAvailability::Offline) {
        return false;
    }
    if reconnecting {
        return false;
    }
    !authority_connected
}

#[cfg(test)]
pub(super) fn should_exit_surface_for_target_presence(
    _spec: &RemoteInteractSurfaceSpec,
    is_present: bool,
    target_exists_in_catalog: bool,
    authority_connected: bool,
    reconnecting: bool,
) -> bool {
    !is_present
        && should_exit_surface_for_target_presence_loss(
            target_exists_in_catalog.then_some(SessionAvailability::Unknown),
            authority_connected,
            reconnecting,
        )
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct RemoteInteractInputSignalDecoder {
    pending: Vec<u8>,
    input_in_progress: bool,
}

impl RemoteInteractInputSignalDecoder {
    pub(super) fn feed(
        &mut self,
        spec: &RemoteInteractSurfaceSpec,
        bytes: &[u8],
    ) -> Vec<RemoteInteractSignal> {
        self.pending.extend_from_slice(bytes);
        let mut signals = Vec::new();

        loop {
            if self.pending.is_empty() {
                break;
            }

            if spec.console_location == ConsoleLocation::ServerConsole
                && self.pending.first() == Some(&0x1d)
            {
                self.pending.drain(..1);
                self.input_in_progress = false;
                signals.push(RemoteInteractSignal::ManualReturnToPicker);
                continue;
            }

            if self.pending.starts_with(b"\x1bOM") {
                self.pending.drain(..3);
                self.push_submit_signals(&mut signals);
                continue;
            }

            if self.pending.starts_with(b"\x1b[13u") {
                self.pending.drain(..5);
                self.push_submit_signals(&mut signals);
                continue;
            }

            if self.pending.starts_with(b"\r\n") {
                self.pending.drain(..2);
                self.push_submit_signals(&mut signals);
                continue;
            }

            if self.pending.first() == Some(&b'\r') || self.pending.first() == Some(&b'\n') {
                self.pending.drain(..1);
                self.push_submit_signals(&mut signals);
                continue;
            }

            if is_partial_remote_submit_sequence(&self.pending) {
                break;
            }

            self.pending.drain(..1);
            if !self.input_in_progress {
                self.input_in_progress = true;
                signals.push(RemoteInteractSignal::ConsoleInputStarted);
            }
        }

        signals
    }

    fn push_submit_signals(&mut self, signals: &mut Vec<RemoteInteractSignal>) {
        if !self.input_in_progress {
            signals.push(RemoteInteractSignal::ConsoleInputStarted);
        }
        self.input_in_progress = false;
        signals.push(RemoteInteractSignal::ConsoleSubmit);
    }
}

fn is_partial_remote_submit_sequence(pending: &[u8]) -> bool {
    [
        b"\x1b".as_slice(),
        b"\x1b[".as_slice(),
        b"\x1bO".as_slice(),
        b"\x1b[1".as_slice(),
        b"\x1b[13".as_slice(),
    ]
    .iter()
    .any(|pattern| pattern.starts_with(pending))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum RemotePaneEvent {
    Input { bytes: Vec<u8>, raw_forwarded: bool },
    Resize,
    MailboxUpdated,
    AuthorityTransport(AuthorityTransportEvent),
    TargetPresenceChanged(bool),
}

pub(super) struct RemotePaneResizeWatcher {
    _writer: UnixStream,
}

pub(super) struct RemoteRawPtyMailboxReader {
    mailbox: LocalNodeMailbox,
    processed_envelopes: usize,
    last_output_seq: Option<u64>,
}

pub(super) struct RawInputMode {
    pub(super) route: Arc<RawPtyInputRoute>,
    pub(super) registry: RemoteConnectionRegistry,
}

#[derive(Default)]
pub(super) struct RawPtyInputRoute {
    inner: Mutex<Option<RawPtyInputRouteState>>,
    next_input_seq: AtomicU64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RawPtyInputRouteState {
    authority_node_id: String,
    session_id: String,
    target_id: String,
    attachment_id: String,
    console_id: String,
    console_host_id: String,
}

pub(super) struct RemotePaneCursorGuard {
    visible_on_drop: bool,
}

impl RemotePaneCursorGuard {
    pub(super) fn hide() -> io::Result<Self> {
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

impl RemoteRawPtyMailboxReader {
    pub(super) fn new(mailbox: LocalNodeMailbox) -> Self {
        Self {
            mailbox,
            processed_envelopes: 0,
            last_output_seq: None,
        }
    }

    pub(super) fn sync_and_collect_raw(&mut self) -> Result<Vec<u8>, RemoteSocketTransportError> {
        let envelopes = self.mailbox.snapshot_from(self.processed_envelopes);
        let mut raw = Vec::new();
        for envelope in &envelopes {
            match &envelope.payload {
                ControlPlanePayload::MirrorBootstrapChunk(payload) => {
                    raw.extend_from_slice(&payload.output_bytes);
                }
                ControlPlanePayload::MirrorBootstrapComplete(payload) => {
                    if payload.alternate_screen_active {
                        raw.extend_from_slice(b"\x1b[?1049h");
                    }
                    if payload.application_cursor_keys {
                        raw.extend_from_slice(b"\x1b[?1h");
                    }
                    raw.extend_from_slice(if payload.cursor_visible {
                        b"\x1b[?25h".as_slice()
                    } else {
                        b"\x1b[?25l".as_slice()
                    });
                }
                ControlPlanePayload::TargetOutput(payload) => {
                    if let Some(last_output_seq) = self.last_output_seq {
                        if payload.output_seq <= last_output_seq {
                            return Err(RemoteSocketTransportError::new(format!(
                                "remote target output received out-of-order for `{}`: {} after {}",
                                payload.target_id, payload.output_seq, last_output_seq
                            )));
                        }
                    }
                    self.last_output_seq = Some(payload.output_seq);
                    raw.extend_from_slice(&payload.output_bytes);
                }
                ControlPlanePayload::RawPtyOutput(payload) => {
                    if let Some(last_output_seq) = self.last_output_seq {
                        if payload.output_seq <= last_output_seq {
                            return Err(RemoteSocketTransportError::new(format!(
                                "remote raw PTY received out-of-order output for `{}`: {} after {}",
                                payload.target_id, payload.output_seq, last_output_seq
                            )));
                        }
                    }
                    self.last_output_seq = Some(payload.output_seq);
                    raw.extend_from_slice(&payload.output_bytes);
                }
                _ => {}
            }
        }
        self.processed_envelopes += envelopes.len();
        Ok(raw)
    }
}

impl RawPtyInputRoute {
    pub(super) fn activate(
        &self,
        target: &ManagedSessionRecord,
        binding: &RemoteAttachmentBinding,
        console_host_id: &str,
    ) {
        *self
            .inner
            .lock()
            .expect("raw PTY input route mutex should not be poisoned") =
            Some(RawPtyInputRouteState {
                authority_node_id: target.address.authority_id().to_string(),
                session_id: binding.session_id.clone(),
                target_id: binding.target_id.clone(),
                attachment_id: binding.attachment_id.clone(),
                console_id: binding.console_id.clone(),
                console_host_id: console_host_id.to_string(),
            });
    }

    pub(super) fn clear(&self) {
        *self
            .inner
            .lock()
            .expect("raw PTY input route mutex should not be poisoned") = None;
    }

    pub(super) fn send(
        &self,
        registry: &RemoteConnectionRegistry,
        input_bytes: Vec<u8>,
    ) -> Result<bool, RemoteSocketTransportError> {
        if input_bytes.is_empty() {
            return Ok(true);
        }
        // Never forward local chrome navigation escape sequences to the
        // remote PTY. Plain cursor keys are application input and must pass
        // through to the remote shell.
        if is_local_navigation_sequence(&input_bytes) {
            return Ok(false);
        }
        let Some(route) = self
            .inner
            .lock()
            .expect("raw PTY input route mutex should not be poisoned")
            .clone()
        else {
            return Ok(false);
        };
        let Some(connection) = registry.connection_for(&route.authority_node_id) else {
            ERROR_LOG.log(format!(
                "[diag-timing] raw_input_route.send: no connection for authority_node_id={}",
                route.authority_node_id
            ));
            return Ok(false);
        };
        let input_seq = self.next_input_seq.fetch_add(1, Ordering::Relaxed) + 1;
        log_exit_submit_if_detected(&route, input_seq, &input_bytes);
        let payload = RawPtyInputPayload {
            attachment_id: route.attachment_id.clone(),
            session_id: route.session_id.clone(),
            target_id: route.target_id.clone(),
            console_id: route.console_id.clone(),
            console_host_id: route.console_host_id.clone(),
            input_seq,
            input_bytes,
        };
        connection
            .send_raw_pty_input(&payload)
            .map_err(|error| RemoteSocketTransportError::new(error.to_string()))?;
        Ok(true)
    }
}

fn log_exit_submit_if_detected(route: &RawPtyInputRouteState, input_seq: u64, bytes: &[u8]) {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return;
    };
    let submitted = text.ends_with('\r') || text.ends_with('\n');
    if !submitted {
        return;
    }
    let command = text.trim_matches(|ch| ch == '\r' || ch == '\n').trim();
    if command != "exit" {
        return;
    }
    ERROR_LOG.log_exit_latency(format!(
        "[diag-exit] input_exit_enter target={} session={} attachment={} console={} input_seq={} bytes={} stage=input_enter",
        route.target_id,
        route.session_id,
        route.attachment_id,
        route.console_id,
        input_seq,
        bytes.len()
    ));
}

pub(super) fn spawn_input_thread(tx: mpsc::Sender<RemotePaneEvent>, raw_input: RawInputMode) {
    thread::spawn(move || {
        let mut stdin = io::stdin().lock();
        let mut buffer = [0u8; 64];
        loop {
            match stdin.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    let bytes = buffer[..read].to_vec();
                    let raw_forwarded = match raw_input
                        .route
                        .send(&raw_input.registry, bytes.clone())
                    {
                        Ok(forwarded) => forwarded,
                        Err(error) => {
                            ERROR_LOG.log(format!(
                                "[diag-timing] input thread: raw route send failed, falling back to event loop: {error}"
                            ));
                            false
                        }
                    };
                    if tx
                        .send(RemotePaneEvent::Input {
                            bytes,
                            raw_forwarded,
                        })
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

pub(super) fn spawn_resize_watcher(
    tx: mpsc::Sender<RemotePaneEvent>,
) -> io::Result<RemotePaneResizeWatcher> {
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

pub(super) fn spawn_mailbox_watcher(mailbox: LocalNodeMailbox, tx: mpsc::Sender<RemotePaneEvent>) {
    thread::spawn(move || {
        let mut seen = 0usize;
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

pub(super) fn spawn_target_presence_watcher(
    target_registry: TargetRegistryService<DefaultTargetCatalogGateway>,
    backend: EmbeddedTmuxBackend,
    socket_name: String,
    session_name: String,
    target_id: String,
    state: Arc<Mutex<bool>>,
    tx: mpsc::Sender<RemotePaneEvent>,
) {
    thread::spawn(move || {
        let mut last_present = true;
        let mut consecutive_misses = 0usize;
        loop {
            if wait_for_presence_signal(&backend, &socket_name, &session_name).is_err() {
                break;
            }
            let current_target = target_registry.find_target(&target_id).ok().flatten();
            let raw_exists = current_target.is_some();
            let raw_online = target_is_online(current_target.as_ref());
            let is_present = if raw_exists {
                consecutive_misses = 0;
                true
            } else {
                consecutive_misses = consecutive_misses.saturating_add(1);
                consecutive_misses < TARGET_PRESENCE_MISS_GRACE_POLLS
            };
            let is_available = is_present && raw_online;
            {
                let mut guard = state
                    .lock()
                    .expect("target presence mutex should not be poisoned");
                *guard = is_present;
            }
            if is_available != last_present {
                ERROR_LOG.log_exit_latency(format!(
                    "[diag-exit] presence_changed target={} available={} present={} raw_exists={} raw_online={} consecutive_misses={} stage=presence_watcher",
                    target_id,
                    is_available,
                    is_present,
                    raw_exists,
                    raw_online,
                    consecutive_misses
                ));
                last_present = is_available;
                if tx
                    .send(RemotePaneEvent::TargetPresenceChanged(is_available))
                    .is_err()
                {
                    break;
                }
            }
        }
    });
}

fn wait_for_presence_signal(
    backend: &EmbeddedTmuxBackend,
    socket_name: &str,
    session_name: &str,
) -> Result<(), TmuxError> {
    backend.wait_for_chrome_refresh_on_socket(socket_name, session_name)
}

pub(super) fn target_is_present(state: &Arc<Mutex<bool>>) -> bool {
    *state
        .lock()
        .expect("target presence mutex should not be poisoned")
}

pub(super) fn target_is_online(target: Option<&ManagedSessionRecord>) -> bool {
    target.is_some_and(|target| target.availability == SessionAvailability::Online)
}

pub(super) fn target_availability(
    target: Option<&ManagedSessionRecord>,
) -> Option<SessionAvailability> {
    target.map(|target| target.availability)
}

pub(crate) fn apply_authority_envelope(
    remote_runtime: &RemoteMainSlotRuntime,
    target: &ManagedSessionRecord,
    envelope: &ProtocolEnvelope<ControlPlanePayload>,
) -> Result<(), RemoteSocketTransportError> {
    fn require_output_matches_target(
        payload_session_id: &str,
        payload_target_id: &str,
        envelope: &ProtocolEnvelope<ControlPlanePayload>,
        target: &ManagedSessionRecord,
    ) -> bool {
        if envelope.sender_id != target.address.authority_id() {
            return false;
        }
        if output_payload_matches_target(payload_session_id, payload_target_id, target) {
            return true;
        }
        ERROR_LOG.log(format!(
            "dropping authority output for wrong target: expected {}:{}, got {}:{}",
            target.address.session_id(),
            target.address.id().as_str(),
            payload_session_id,
            payload_target_id
        ));
        false
    }

    match &envelope.payload {
        ControlPlanePayload::OpenMirrorAccepted(payload) => {
            remote_runtime.record_mirror_accepted(&payload.session_id);
            Ok(())
        }
        ControlPlanePayload::OpenMirrorRejected(payload) => {
            remote_runtime.record_mirror_rejected(&payload.session_id, payload.message.clone());
            Err(RemoteSocketTransportError::new(format!(
                "remote mirror open rejected for `{}`: {}",
                payload.target_id, payload.message
            )))
        }
        ControlPlanePayload::TargetOutput(payload) => {
            if !require_output_matches_target(
                payload.session_id.as_str(),
                payload.target_id.as_str(),
                envelope,
                target,
            ) {
                return Ok(());
            }
            remote_runtime
                .send_target_output(
                    target,
                    payload.output_seq,
                    payload.stream,
                    payload.output_bytes.clone(),
                )
                .map_err(|error| RemoteSocketTransportError::new(error.to_string()))
        }
        ControlPlanePayload::RawPtyOutput(payload) => {
            if !require_output_matches_target(
                payload.session_id.as_str(),
                payload.target_id.as_str(),
                envelope,
                target,
            ) {
                return Ok(());
            }
            remote_runtime
                .send_raw_pty_output(target, payload.output_seq, payload.output_bytes.clone())
                .map_err(|error| RemoteSocketTransportError::new(error.to_string()))
        }
        ControlPlanePayload::MirrorBootstrapChunk(payload) => {
            if !require_output_matches_target(
                payload.session_id.as_str(),
                payload.target_id.as_str(),
                envelope,
                target,
            ) {
                return Ok(());
            }
            remote_runtime
                .send_mirror_bootstrap_chunk(
                    target,
                    payload.chunk_seq,
                    payload.stream,
                    payload.output_bytes.clone(),
                )
                .map_err(|error| RemoteSocketTransportError::new(error.to_string()))
        }
        ControlPlanePayload::MirrorBootstrapComplete(payload) => {
            if !require_output_matches_target(
                payload.session_id.as_str(),
                payload.target_id.as_str(),
                envelope,
                target,
            ) {
                return Ok(());
            }
            remote_runtime
                .send_mirror_bootstrap_complete(
                    target,
                    payload.last_chunk_seq,
                    payload.alternate_screen_active,
                    payload.application_cursor_keys,
                    payload.cursor_visible,
                )
                .map_err(|error| RemoteSocketTransportError::new(error.to_string()))
        }
        ControlPlanePayload::TargetExited(payload) => {
            ERROR_LOG.log_exit_latency(format!(
                "[diag-exit] authority_target_exited target={} session={} source_session={:?} sender={} stage=authority_envelope",
                target.address.qualified_target(),
                payload.transport_session_id,
                payload.source_session_name,
                envelope.sender_id
            ));
            // Authority explicitly signalled session exit — return a
            // distinguished error so the event loop can perform a clean
            // shutdown instead of entering reconnection.
            Err(RemoteSocketTransportError::new(
                "authority signalled session exit",
            ))
        }
        other => Err(RemoteSocketTransportError::new(format!(
            "unexpected authority envelope payload `{}`",
            other.message_type()
        ))),
    }
}

pub(super) fn draw_remote_snapshot(
    terminal: &TerminalRuntime,
    target: &ManagedSessionRecord,
    binding: Option<&RemoteAttachmentBinding>,
    snapshot: &RemoteObserverSnapshot,
    authority_status: &AuthorityTransportStatus,
    initial_connecting_elapsed: Option<Duration>,
    reconnecting_elapsed: Option<Duration>,
    reconnect_animation_frame: u8,
) -> Result<(), LifecycleError> {
    let viewport = terminal.current_size_or_default();
    // Keep showing last known content when authority is disconnected due to
    // network jitter but we have received output before. Only show placeholder
    // when we never got any content or are still waiting for initial authority.
    let has_content = snapshot.has_visible_output
        && !matches!(
            authority_status,
            AuthorityTransportStatus::WaitingForRemoteAuthority
        );
    let active_screen = snapshot.active_screen();
    let placeholder =
        (!has_content).then(|| placeholder_lines(target, binding, authority_status, viewport));
    let mut rendered_lines = Vec::with_capacity(usize::from(viewport.rows.max(1)));
    for row in 0..usize::from(viewport.rows.max(1)) {
        rendered_lines.push(if has_content {
            render_terminal_safe_remote_line(
                active_screen
                    .styled_lines
                    .get(row)
                    .map(String::as_str)
                    .unwrap_or(""),
                active_screen
                    .lines
                    .get(row)
                    .map(String::as_str)
                    .unwrap_or(""),
            )
        } else {
            trim_trailing_padding(
                placeholder
                    .as_ref()
                    .and_then(|lines| lines.get(row))
                    .map(String::as_str)
                    .unwrap_or(""),
            )
            .to_string()
        });
    }
    // Overlay initial connecting status bar on the last row
    if let Some(elapsed) = initial_connecting_elapsed {
        let last_row = rendered_lines.len().saturating_sub(1);
        rendered_lines[last_row] =
            render_initial_connecting_status(viewport, elapsed, reconnect_animation_frame);
    }
    // Overlay reconnecting status bar on the last row when reconnecting
    if let Some(elapsed) = reconnecting_elapsed {
        let last_row = rendered_lines.len().saturating_sub(1);
        rendered_lines[last_row] =
            render_reconnecting_status(viewport, elapsed, reconnect_animation_frame);
    }
    debug_log_draw_snapshot(
        "draw_remote_snapshot",
        &target.address.qualified_target(),
        snapshot.last_output_seq,
        has_content,
        active_screen.lines.as_slice(),
        rendered_lines.as_slice(),
        &format!(
            "cursor_row={} cursor_col={} cursor_visible={} has_visible_output={} alt_screen={}",
            active_screen.cursor_row,
            active_screen.cursor_col,
            active_screen.cursor_visible,
            snapshot.has_visible_output,
            active_screen.alternate_screen,
        ),
    );

    let mut output = session_stdout();
    // Hide cursor and disable line wrapping before redraw.
    write!(output, "\x1b[?25l\x1b[?7l").map_err(|error| {
        LifecycleError::Io(
            "failed to hide remote main-slot cursor before redraw".to_string(),
            error,
        )
    })?;
    for row in 0..usize::from(viewport.rows.max(1)) {
        let line = rendered_lines.get(row).map(String::as_str).unwrap_or("");
        write!(output, "\x1b[{};1H\x1b[2K{}", row + 1, line).map_err(|error| {
            LifecycleError::Io("failed to draw remote main-slot output".to_string(), error)
        })?;
    }

    // Only show cursor when we're connected and showing live content
    if has_content
        && reconnecting_elapsed.is_none()
        && matches!(authority_status, AuthorityTransportStatus::Connected)
    {
        render_remote_cursor(&mut output, active_screen)?;
    } else {
        write!(output, "\x1b[?7h\x1b[?25l").map_err(|error| {
            LifecycleError::Io("failed to hide remote main-slot cursor".to_string(), error)
        })?;
    }
    output.flush().map_err(|error| {
        LifecycleError::Io("failed to flush remote main-slot output".to_string(), error)
    })
}

fn render_reconnecting_status(
    viewport: TerminalSize,
    elapsed: Duration,
    animation_frame: u8,
) -> String {
    let spinner =
        RECONNECT_SPINNER_FRAMES[animation_frame as usize % RECONNECT_SPINNER_FRAMES.len()];
    let secs = elapsed.as_secs();
    let text = format!(" {} reconnecting... {}s ", spinner, secs);
    let width = usize::from(viewport.cols.max(1));
    if text.chars().count() >= width {
        return text.chars().take(width).collect();
    }
    let padded = format!("{text:<width$}");
    // Use amber background with black text for visibility
    format!("\x1b[48;5;220m\x1b[30m{padded}\x1b[0m")
}

fn render_initial_connecting_status(
    viewport: TerminalSize,
    elapsed: Duration,
    animation_frame: u8,
) -> String {
    let spinner =
        RECONNECT_SPINNER_FRAMES[animation_frame as usize % RECONNECT_SPINNER_FRAMES.len()];
    let secs = elapsed.as_secs();
    let text = format!(" {} connecting to remote... {}s ", spinner, secs);
    let width = usize::from(viewport.cols.max(1));
    if text.chars().count() >= width {
        return text.chars().take(width).collect();
    }
    let padded = format!("{text:<width$}");
    // Use blue background with black text for visibility
    format!("\x1b[48;5;33m\x1b[30m{padded}\x1b[0m")
}

fn render_remote_cursor(
    output: &mut impl Write,
    active_screen: &ScreenSnapshot,
) -> Result<(), LifecycleError> {
    let display_row = usize::from(active_screen.cursor_row.saturating_add(1));
    let display_col = usize::from(active_screen.cursor_col.saturating_add(1));
    write!(
        output,
        "\x1b[?7h\x1b[{};{}H{}",
        display_row,
        display_col,
        if active_screen.cursor_visible {
            "\x1b[?25h"
        } else {
            "\x1b[?25l"
        },
    )
    .map_err(|error| {
        LifecycleError::Io("failed to sync remote main-slot cursor".to_string(), error)
    })
}

fn debug_log_draw_snapshot(
    stage: &str,
    target: &str,
    last_output_seq: Option<u64>,
    connected_visible_output: bool,
    lines: &[String],
    rendered_lines: &[String],
    cursor_state: &str,
) {
    let Ok(path) = std::env::var("WAITAGENT_REMOTE_DEBUG_LOG") else {
        return;
    };
    let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) else {
        return;
    };
    let draw_seq = REMOTE_DRAW_DEBUG_SEQ.fetch_add(1, Ordering::Relaxed) + 1;

    let _ = writeln!(
        file,
        "[{stage}] seq={draw_seq} target={target} last_output_seq={last_output_seq:?} connected_visible_output={connected_visible_output} {cursor_state}"
    );
    for (index, line) in lines.iter().take(12).enumerate() {
        let rendered = rendered_lines.get(index).map(String::as_str).unwrap_or("");
        let _ = writeln!(
            file,
            "L{:02}: plain={:?} rendered={:?}",
            index + 1,
            line,
            rendered
        );
    }
    let _ = writeln!(file);
}

pub(super) fn render_terminal_safe_remote_line(styled_line: &str, plain_line: &str) -> String {
    let visible_width = trimmed_display_width(plain_line);
    if visible_width == 0 {
        return String::new();
    }

    let mut rendered = String::new();
    let mut index = 0;
    let mut consumed_width = 0;
    let mut saw_escape = false;

    while index < styled_line.len() && consumed_width < visible_width {
        let remaining = &styled_line[index..];
        if remaining.as_bytes().first() == Some(&0x1b) {
            let escape_len = next_ansi_escape_len(remaining);
            rendered.push_str(&remaining[..escape_len]);
            index += escape_len;
            saw_escape = true;
            continue;
        }

        let Some(ch) = remaining.chars().next() else {
            break;
        };
        let ch_width = terminal_char_display_width(ch);
        if ch_width == 0 {
            index += ch.len_utf8();
            continue;
        }
        if consumed_width + ch_width > visible_width {
            break;
        }

        rendered.push(ch);
        consumed_width += ch_width;
        index += ch.len_utf8();
    }

    if saw_escape && !rendered.ends_with("\x1b[0m") {
        rendered.push_str("\x1b[0m");
    }

    rendered
}

pub(super) fn next_ansi_escape_len(input: &str) -> usize {
    let bytes = input.as_bytes();
    if bytes.len() < 2 || bytes[0] != 0x1b {
        return input.chars().next().map(char::len_utf8).unwrap_or(0);
    }

    match bytes[1] {
        b'[' => {
            2 + bytes[2..]
                .iter()
                .position(|byte| (0x40..=0x7e).contains(byte))
                .map(|offset| offset + 1)
                .unwrap_or(bytes.len().saturating_sub(2))
        }
        b']' => {
            for index in 2..bytes.len() {
                if bytes[index] == 0x07 {
                    return index + 1;
                }
                if bytes[index] == 0x1b && bytes.get(index + 1) == Some(&b'\\') {
                    return index + 2;
                }
            }
            bytes.len()
        }
        _ => 2,
    }
}

fn trim_trailing_padding(line: &str) -> &str {
    line.trim_end_matches(' ')
}

fn trimmed_display_width(line: &str) -> usize {
    trim_trailing_padding(line)
        .chars()
        .map(terminal_char_display_width)
        .sum()
}

fn terminal_char_display_width(ch: char) -> usize {
    if ch.is_control() {
        0
    } else if matches!(
        ch as u32,
        0x1100..=0x115F
            | 0x2329..=0x232A
            | 0x2E80..=0xA4CF
            | 0xAC00..=0xD7A3
            | 0xF900..=0xFAFF
            | 0xFE10..=0xFE19
            | 0xFE30..=0xFE6F
            | 0xFF00..=0xFF60
            | 0xFFE0..=0xFFE6
            | 0x1F300..=0x1FAFF
    ) {
        2
    } else {
        1
    }
}

pub(super) fn placeholder_lines(
    target: &ManagedSessionRecord,
    binding: Option<&RemoteAttachmentBinding>,
    authority_status: &AuthorityTransportStatus,
    viewport: TerminalSize,
) -> Vec<String> {
    let (status_label, detail_line) = match authority_status {
        AuthorityTransportStatus::WaitingForRemoteAuthority => (
            "waiting for remote authority",
            "waiting for a live authority node to register this target transport".to_string(),
        ),
        AuthorityTransportStatus::Connected => (
            "connected",
            "authority transport is live; waiting for remote target output".to_string(),
        ),
        AuthorityTransportStatus::Disconnected => (
            "disconnected",
            "authority transport disconnected; waiting for the remote authority to come back"
                .to_string(),
        ),
        AuthorityTransportStatus::Failed(message) => {
            ("failed", format!("authority transport error: {message}"))
        }
    };
    let mut lines = vec![
        format!(
            "remote target {}",
            target
                .command_name
                .as_deref()
                .unwrap_or(target.address.session_id())
        ),
        format!("target-id: {}", target.address.id().as_str()),
        format!(
            "attachment: {}",
            binding
                .map(|binding| binding.attachment_id.as_str())
                .unwrap_or("pending")
        ),
        format!("authority transport: {status_label}"),
    ];
    lines.push(detail_line);

    while lines.len() < usize::from(viewport.rows.max(1)) {
        lines.push(String::new());
    }
    lines
}

pub(super) fn authority_status_from_runtime(
    remote_runtime: &RemoteMainSlotRuntime,
    target: &ManagedSessionRecord,
    target_is_present: bool,
    waiting_status: &AuthorityTransportStatus,
) -> AuthorityTransportStatus {
    if !target_is_present {
        return AuthorityTransportStatus::Disconnected;
    }
    if remote_runtime.has_connection(target.address.authority_id()) {
        AuthorityTransportStatus::Connected
    } else {
        waiting_status.clone()
    }
}

pub(super) fn authority_transport_event_sender(
    tx: mpsc::Sender<RemotePaneEvent>,
) -> mpsc::Sender<AuthorityTransportEvent> {
    let (authority_tx, authority_rx) = mpsc::channel();
    thread::spawn(move || {
        while let Ok(event) = authority_rx.recv() {
            if tx.send(RemotePaneEvent::AuthorityTransport(event)).is_err() {
                break;
            }
        }
    });
    authority_tx
}

pub(crate) fn main_slot_surface_spec(command: &RemoteMainSlotCommand) -> RemoteInteractSurfaceSpec {
    RemoteInteractSurfaceSpec {
        socket_name: command.socket_name.clone(),
        surface_scope: command.session_name.clone(),
        target: command.target.clone(),
        console_id: main_slot_console_id(command),
        console_host_id: command.socket_name.clone(),
        console_location: ConsoleLocation::LocalWorkspace,
    }
}

pub(super) fn main_slot_console_id(command: &RemoteMainSlotCommand) -> String {
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

/// Escape sequences for local chrome navigation keys that must not be
/// forwarded to the remote PTY.
pub(super) fn is_local_navigation_sequence(bytes: &[u8]) -> bool {
    // C-Right (focus sidebar): CSI 1;5 C
    bytes == b"\x1b[1;5C"
}

/// Execute a tmux `select-pane` command for local chrome navigation.
/// Called from the event loop when a navigation escape sequence is received
/// and the raw input route refused to forward it.
pub(super) fn try_local_navigation(socket_name: &str, bytes: &[u8]) {
    let direction = if bytes == b"\x1b[1;5C" {
        "-R"
    } else {
        return;
    };
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    let tmux = std::path::PathBuf::from(home).join(".local/share/waitagent/tmux");
    if std::process::Command::new(&tmux)
        .args(["-L", socket_name, "select-pane", direction])
        .output()
        .is_err()
    {
        ERROR_LOG.log(format!(
            "[diag] local navigation failed: tmux={} socket={} direction={}",
            tmux.display(),
            socket_name,
            direction
        ));
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemoteSocketTransportError {
    message: String,
}

impl RemoteSocketTransportError {
    pub(super) fn new(message: impl Into<String>) -> Self {
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

pub(super) fn remote_protocol_error(error: impl ToString) -> LifecycleError {
    LifecycleError::Protocol(error.to_string())
}

pub(super) fn remote_pane_error<E>(error: E) -> LifecycleError
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
