use crate::cli::RemoteNetworkConfig;
use crate::domain::session_catalog::ManagedSessionRecord;
use crate::infra::error_log::ERROR_LOG;
use crate::infra::remote_protocol::{
    AgentSessionEntryPayload, ApplyResizePayload, BootstrapMode, ControlPlanePayload,
    ListAgentSessionsRequestPayload, OpenMirrorRequestPayload, PasteFileRequestPayload,
    ProtocolEnvelope, RawPtyInputPayload, REMOTE_PROTOCOL_VERSION,
};
use crate::infra::remote_transport_codec::{
    read_authority_transport_frame, write_authority_transport_frame, AuthorityTransportFrame,
};
use crate::lifecycle::LifecycleError;
use crate::ratatui_node::runtime::SharedState;
use crate::ratatui_node::state_event::{AgentSessionEntry, StateEvent};
use crate::remote::authority::remote_authority_transport_runtime::authority_transport_socket_path;
use crate::remote::node::remote_node_ingress_server_runtime::notify_authority_socket_ready;
use crate::remote::node::remote_node_transport_runtime::{read_client_hello, write_server_hello};
use crate::remote::observer::RemoteObserverRuntime;
use crate::remote::publication::remote_transport_runtime::LocalNodeMailbox;
use std::io::Write;
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const TASK_STATE_INPUT: u8 = 0;
const TASK_STATE_RUNNING: u8 = 1;
const REMOTE_IDLE_INPUT_MS: u128 = 500;

/// A remote session viewed by the local ratatui node.
///
/// It opens a mirror over the local authority transport socket, receives raw
/// PTY output through the gRPC bridge, and renders it with `RemoteObserverRuntime`.
pub struct RatatuiRemoteSession {
    pub target_id: String,
    pub session_id: String,
    pub authority_node_id: String,
    observer: Mutex<RemoteObserverRuntime>,
    writer: Mutex<Option<UnixStream>>,
    listener: Mutex<Option<UnixListener>>,
    running: Arc<AtomicBool>,
    closed: AtomicBool,
    opened: AtomicBool,
    next_input_seq: AtomicU64,
    initial_cols: Mutex<u16>,
    initial_rows: Mutex<u16>,
    shared: Arc<SharedState>,
    /// Optional one-shot channel signaled when the authority transport handshake
    /// completes or fails. Used by the reconnect worker to wait for the new
    /// transport without polling.
    connected_tx: Mutex<Option<mpsc::Sender<Result<(), LifecycleError>>>>,
    /// Milliseconds since UNIX epoch when the last PTY output frame arrived.
    /// Used to infer Running/Input state for the sidebar badge.
    last_output_ms: AtomicU64,
    /// Current inferred task state: 0 = input, 1 = running.
    task_state: AtomicU8,
}

impl std::fmt::Debug for RatatuiRemoteSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RatatuiRemoteSession")
            .field("target_id", &self.target_id)
            .field("session_id", &self.session_id)
            .field("authority_node_id", &self.authority_node_id)
            .finish_non_exhaustive()
    }
}

impl RatatuiRemoteSession {
    /// Start listening on the authority transport socket, register it with the
    /// local ingress owner, and return the session. The acceptor sends the
    /// OpenMirrorRequest once the gRPC bridge connects.
    ///
    /// If `connection_tx` is provided, it is signaled once the authority
    /// transport handshake completes (Ok) or fails (Err). This lets callers
    /// such as the reconnect worker block until the transport is ready.
    pub fn open(
        target: &ManagedSessionRecord,
        socket_name: &str,
        network: &RemoteNetworkConfig,
        shared: &Arc<SharedState>,
        connection_tx: Option<mpsc::Sender<Result<(), LifecycleError>>>,
    ) -> Result<Arc<Self>, LifecycleError> {
        let target_id = target.address.qualified_target();
        let session_id = target.address.session_id().to_string();
        let authority_node_id = target.address.authority_id().to_string();
        let socket_path = authority_transport_socket_path(socket_name, &session_id, &target_id);

        if socket_path.exists() {
            crate::infra::best_effort::remove_file(&socket_path);
        }
        if let Some(parent) = socket_path.parent() {
            crate::infra::best_effort::create_dir_all(parent);
        }
        let listener = UnixListener::bind(&socket_path).map_err(|error| {
            LifecycleError::Io(
                format!(
                    "failed to bind ratatui remote authority socket {}",
                    socket_path.display()
                ),
                error,
            )
        })?;

        let observer = RemoteObserverRuntime::new(LocalNodeMailbox::default(), 80, 24);
        let session = Arc::new(Self {
            target_id: target_id.clone(),
            session_id: session_id.clone(),
            authority_node_id: authority_node_id.clone(),
            observer: Mutex::new(observer),
            writer: Mutex::new(None),
            listener: Mutex::new(Some(listener)),
            running: Arc::new(AtomicBool::new(true)),
            closed: AtomicBool::new(false),
            opened: AtomicBool::new(false),
            next_input_seq: AtomicU64::new(1),
            initial_cols: Mutex::new(80),
            initial_rows: Mutex::new(24),
            shared: shared.clone(),
            connected_tx: Mutex::new(connection_tx),
            last_output_ms: AtomicU64::new(now_millis() as u64),
            task_state: AtomicU8::new(TASK_STATE_INPUT),
        });

        // Start the acceptor first so the ingress owner can connect back and
        // complete the authority transport handshake immediately.
        spawn_authority_transport_acceptor(
            session.clone(),
            target_id.clone(),
            session_id.clone(),
            authority_node_id.clone(),
        );

        if let Err(error) = notify_authority_socket_ready(network, &authority_node_id, &socket_path)
        {
            ERROR_LOG.log(format!(
                "[ratatui-remote-session] failed to notify ingress owner for {target_id}: {error}"
            ));
        }

        Ok(session)
    }

    /// Interrupt the authority acceptor and reader threads and close the
    /// underlying socket so the session stops immediately.
    ///
    /// Marks the session as explicitly closed so the reader thread does not
    /// emit a disconnect event; the caller (StateEventLoop) already owns the
    /// lifecycle decision.
    pub fn stop(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.running.store(false, Ordering::Relaxed);

        // Dropping the listener unblocks the acceptor thread if it is blocked
        // on accept().
        let _ = self
            .listener
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();

        // Shutting down the writer stream unblocks the reader thread if it is
        // blocked on read_authority_transport_frame().
        let mut guard = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(writer) = guard.take() {
            let _ = writer.shutdown(Shutdown::Both);
        }
    }

    /// Clear the observer output sequence watermark so a newly reconnected
    /// authority transport can resume feeding without its initial frames being
    /// dropped as duplicates.
    pub fn clear_output_seq(&self) {
        let mut observer = self.observer.lock().unwrap_or_else(|e| e.into_inner());
        observer.clear_output_seq();
    }

    /// Send an OpenMirrorRequest once the local terminal size is known.
    /// If the ingress bridge has not connected yet, the size is stored and
    /// flushed automatically when the bridge is ready.
    pub fn send_open_mirror(&self, cols: u16, rows: u16) {
        ERROR_LOG.log(format!(
            "[timing] remote send_open_mirror target={} cols={cols} rows={rows}",
            self.target_id
        ));
        {
            let mut initial_cols = self.initial_cols.lock().unwrap_or_else(|e| e.into_inner());
            let mut initial_rows = self.initial_rows.lock().unwrap_or_else(|e| e.into_inner());
            *initial_cols = cols;
            *initial_rows = rows;
        }
        let mut guard = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        let Some(writer) = guard.as_mut() else {
            ERROR_LOG.log(format!(
                "[timing] remote send_open_mirror target={} writer=None skipped",
                self.target_id
            ));
            return;
        };
        let payload = OpenMirrorRequestPayload {
            session_id: self.session_id.clone(),
            target_id: self.target_id.clone(),
            console_id: format!("ratatui-console-{}", std::process::id()),
            cols: cols as usize,
            rows: rows as usize,
            raw_pty_passthrough: true,
            bootstrap_mode: BootstrapMode::VisibleOnly,
        };
        let now = now_millis();
        let timestamp = format!("{now}Z");
        let envelope = ProtocolEnvelope {
            protocol_version: REMOTE_PROTOCOL_VERSION.to_string(),
            message_id: format!("ratatui-open-mirror-{now}"),
            message_type: "open_mirror_request",
            timestamp,
            sender_id: self.authority_node_id.clone(),
            correlation_id: None,
            session_id: Some(self.session_id.clone()),
            target_id: Some(self.target_id.clone()),
            attachment_id: None,
            console_id: Some(payload.console_id.clone()),
            payload: ControlPlanePayload::OpenMirrorRequest(payload),
        };
        let _ = write_authority_transport_frame(
            writer,
            &AuthorityTransportFrame::ControlPlane(Box::new(envelope)),
        );
        let _ = writer.flush();
        ERROR_LOG.log(format!(
            "[timing] remote send_open_mirror target={} writer flushed",
            self.target_id
        ));
    }

    /// Return whether the remote mirror has already been opened.
    pub fn is_opened(&self) -> bool {
        self.opened.load(Ordering::SeqCst)
    }

    /// Open the remote mirror with the given terminal size exactly once.
    ///
    /// This is the only place that should transition a session from "created"
    /// to "opened".  It stores the size, sends the OpenMirrorRequest, and
    /// resizes the local observer so the first rendered frame already has the
    /// correct dimensions.
    pub fn open_mirror(&self, cols: u16, rows: u16) {
        ERROR_LOG.log(format!(
            "[timing] remote open_mirror target={} cols={cols} rows={rows}",
            self.target_id
        ));
        if self
            .opened
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::Relaxed)
            .is_err()
        {
            ERROR_LOG.log(format!(
                "[timing] remote open_mirror target={} already opened",
                self.target_id
            ));
            return;
        }
        self.send_open_mirror(cols, rows);
        self.resize_local_screen(cols, rows);
    }

    /// Flush a pending OpenMirrorRequest using the last stored size.
    ///
    /// This is called by the authority transport reader once the bridge
    /// connects.  If the mirror has not been opened yet we must not send a
    /// default 80x24 request; the real size will be supplied by the first
    /// resize/activate from the TUI.
    fn flush_open_mirror(&self) {
        ERROR_LOG.log(format!(
            "[timing] remote flush_open_mirror target={}",
            self.target_id
        ));
        if !self.opened.load(Ordering::SeqCst) {
            ERROR_LOG.log(format!(
                "[timing] remote flush_open_mirror target={} not opened",
                self.target_id
            ));
            return;
        }
        let (cols, rows) = {
            let cols = *self.initial_cols.lock().unwrap_or_else(|e| e.into_inner());
            let rows = *self.initial_rows.lock().unwrap_or_else(|e| e.into_inner());
            (cols, rows)
        };
        self.send_open_mirror(cols, rows);
    }

    /// Forward keyboard input to the remote session.
    ///
    /// The remote PTY handles echo, so the modeled screen stays in sync with
    /// the remote readline state.
    pub fn feed_input(&self, bytes: impl Into<Vec<u8>>) {
        let bytes = bytes.into();
        let seq = self.next_input_seq.fetch_add(1, Ordering::Relaxed);
        let mut guard = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        let Some(writer) = guard.as_mut() else {
            ERROR_LOG.log(format!(
                "[ratatui-remote-session] feed_input dropped for {}: writer not ready",
                self.target_id
            ));
            return;
        };
        ERROR_LOG.log(format!(
            "[ratatui-remote-session] feed_input target={} seq={} bytes={} hex={}",
            self.target_id,
            seq,
            bytes.len(),
            bytes
                .iter()
                .map(|b| format!("\\x{b:02x}"))
                .collect::<Vec<_>>()
                .join("")
        ));
        let payload = RawPtyInputPayload {
            session_id: self.session_id.clone(),
            target_id: self.target_id.clone(),
            attachment_id: format!("ratatui-attach-{}", std::process::id()),
            console_id: format!("ratatui-console-{}", std::process::id()),
            console_host_id: self.authority_node_id.clone(),
            input_seq: seq,
            input_bytes: bytes.clone(),
        };
        let frame = AuthorityTransportFrame::RawPtyInput(payload);
        match write_authority_transport_frame(writer, &frame) {
            Ok(_) => {
                let flush_result = writer.flush();
                ERROR_LOG.log(format!(
                    "[ratatui-remote-session] feed_input target={} seq={} write_ok flush={flush_result:?}",
                    self.target_id, seq
                ));
            }
            Err(error) => {
                ERROR_LOG.log(format!(
                    "[ratatui-remote-session] feed_input target={} seq={} write_failed: {error}",
                    self.target_id, seq
                ));
            }
        }
    }

    /// Forward a terminal resize to the remote session.
    pub fn resize(&self, cols: u16, rows: u16) {
        ERROR_LOG.log(format!(
            "[ratatui-remote-session] send apply_resize cols={cols} rows={rows}"
        ));
        let mut guard = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        let Some(writer) = guard.as_mut() else {
            return;
        };
        let payload = ApplyResizePayload {
            session_id: self.session_id.clone(),
            target_id: self.target_id.clone(),
            resize_epoch: 1,
            resize_authority_console_id: format!("ratatui-console-{}", std::process::id()),
            cols: cols as usize,
            rows: rows as usize,
        };
        let now = now_millis();
        let timestamp = format!("{now}Z");
        let envelope = ProtocolEnvelope {
            protocol_version: REMOTE_PROTOCOL_VERSION.to_string(),
            message_id: format!("ratatui-resize-{now}"),
            message_type: "apply_resize",
            timestamp,
            sender_id: self.authority_node_id.clone(),
            correlation_id: None,
            session_id: Some(self.session_id.clone()),
            target_id: Some(self.target_id.clone()),
            attachment_id: None,
            console_id: Some(payload.resize_authority_console_id.clone()),
            payload: ControlPlanePayload::ApplyResize(payload),
        };
        let _ = write_authority_transport_frame(
            writer,
            &AuthorityTransportFrame::ControlPlane(Box::new(envelope)),
        );
        let _ = writer.flush();
    }

    /// Forward a pasted file to the remote session.
    ///
    /// The file is split into ~1 MiB chunks and sent as `PasteFileRequest`
    /// control-plane envelopes. The remote peer reassembles the chunks, caches
    /// the file under its own `/tmp/waitagent/`, and injects the cached path
    /// into the target shell as keyboard input.
    pub fn send_paste_file(&self, filename_hint: &str, bytes: &[u8]) {
        const CHUNK_SIZE: usize = 1024 * 1024;
        let file_id = generate_paste_file_id();
        let total_chunks = if bytes.is_empty() {
            1
        } else {
            bytes.len().div_ceil(CHUNK_SIZE) as u32
        };

        ERROR_LOG.log(format!(
            "[ratatui-remote-session] send_paste_file target={} file_id={file_id} bytes={} chunks={total_chunks}",
            self.target_id,
            bytes.len()
        ));

        // Encode all chunks into a single buffer before acquiring the writer lock
        // so other input is not blocked while we slice and frame a large file.
        let timestamp = format!("{}Z", now_millis());
        let mut encoded = Vec::with_capacity(bytes.len().saturating_add(bytes.len() / 8));
        for chunk_index in 0..total_chunks {
            let start = (chunk_index as usize) * CHUNK_SIZE;
            let end = (start + CHUNK_SIZE).min(bytes.len());
            let chunk_bytes = bytes[start..end].to_vec();
            let payload = PasteFileRequestPayload {
                session_id: self.session_id.clone(),
                target_id: self.target_id.clone(),
                filename_hint: filename_hint.to_string(),
                file_id: file_id.clone(),
                total_chunks,
                chunk_index,
                chunk_bytes,
            };
            let envelope = ProtocolEnvelope {
                protocol_version: REMOTE_PROTOCOL_VERSION.to_string(),
                message_id: format!("ratatui-paste-file-{file_id}-{chunk_index}-{timestamp}"),
                message_type: "paste_file_request",
                timestamp: timestamp.clone(),
                sender_id: self.authority_node_id.clone(),
                correlation_id: None,
                session_id: Some(self.session_id.clone()),
                target_id: Some(self.target_id.clone()),
                attachment_id: None,
                console_id: None,
                payload: ControlPlanePayload::PasteFileRequest(payload),
            };
            if let Err(error) = write_authority_transport_frame(
                &mut encoded,
                &AuthorityTransportFrame::ControlPlane(Box::new(envelope)),
            ) {
                ERROR_LOG.log(format!(
                    "[ratatui-remote-session] send_paste_file target={} chunk={chunk_index} encode_failed: {error}",
                    self.target_id
                ));
                return;
            }
        }

        let mut guard = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        let Some(writer) = guard.as_mut() else {
            ERROR_LOG.log(format!(
                "[ratatui-remote-session] send_paste_file dropped for {}: writer not ready",
                self.target_id
            ));
            return;
        };
        if let Err(error) = writer.write_all(&encoded) {
            ERROR_LOG.log(format!(
                "[ratatui-remote-session] send_paste_file target={} write_failed: {error}",
                self.target_id
            ));
            return;
        }
        if let Err(error) = writer.flush() {
            ERROR_LOG.log(format!(
                "[ratatui-remote-session] send_paste_file target={} flush_failed: {error}",
                self.target_id
            ));
        }
    }

    /// Send a ListAgentSessionsRequest to the remote peer for this session.
    pub fn send_list_agent_sessions(&self, request_id: &str, agent: &str) {
        let mut guard = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        let Some(writer) = guard.as_mut() else {
            ERROR_LOG.log(format!(
                "[ratatui-remote-session] send_list_agent_sessions dropped for {}: writer not ready",
                self.target_id
            ));
            return;
        };
        let payload = ListAgentSessionsRequestPayload {
            request_id: request_id.to_string(),
            target_id: self.target_id.clone(),
            agent: agent.to_string(),
        };
        let now = now_millis();
        let envelope = ProtocolEnvelope {
            protocol_version: REMOTE_PROTOCOL_VERSION.to_string(),
            message_id: format!("ratatui-list-agent-sessions-{now}"),
            message_type: "list_agent_sessions_request",
            timestamp: format!("{now}Z"),
            sender_id: self.authority_node_id.clone(),
            correlation_id: Some(request_id.to_string()),
            session_id: Some(self.session_id.clone()),
            target_id: Some(self.target_id.clone()),
            attachment_id: None,
            console_id: None,
            payload: ControlPlanePayload::ListAgentSessionsRequest(payload),
        };
        if let Err(error) = write_authority_transport_frame(
            writer,
            &AuthorityTransportFrame::ControlPlane(Box::new(envelope)),
        ) {
            ERROR_LOG.log(format!(
                "[ratatui-remote-session] send_list_agent_sessions target={} encode_failed: {error}",
                self.target_id
            ));
            return;
        }
        let _ = writer.flush();
    }

    /// Snapshot the rendered screen as plain/styled text lines and the cursor position.
    pub fn snapshot(&self) -> (Vec<String>, Vec<String>, Option<(u16, u16)>) {
        let mut observer = self.observer.lock().unwrap_or_else(|e| e.into_inner());
        let _ = observer.sync();
        let snap = observer.snapshot();
        let screen = snap.active_screen();
        let cursor = if screen.cursor_row < screen.size.rows && screen.cursor_col < screen.size.cols
        {
            Some((screen.cursor_col, screen.cursor_row))
        } else {
            None
        };
        (screen.lines.clone(), screen.styled_lines.clone(), cursor)
    }

    /// Return the full scrollback history plus the visible screen as plain/styled lines.
    pub fn history_snapshot(&self) -> (Vec<String>, Vec<String>) {
        let mut observer = self.observer.lock().unwrap_or_else(|e| e.into_inner());
        let _ = observer.sync();
        let snap = observer.snapshot();
        let screen = snap.active_screen();
        let mut lines = screen.styled_scrollback.clone();
        let mut styled_lines = screen.styled_scrollback.clone();
        lines.extend_from_slice(&screen.lines);
        styled_lines.extend_from_slice(&screen.styled_lines);
        (lines, styled_lines)
    }

    /// Return the terminal-mode flags needed to translate logical keys into
    /// the byte sequences expected by the remote PTY.
    pub fn translation_mode(&self) -> crate::ratatui_node::key_translation::KeyTranslationMode {
        let observer = self.observer.lock().unwrap_or_else(|e| e.into_inner());
        crate::ratatui_node::key_translation::KeyTranslationMode {
            application_cursor_keys: observer.application_cursor_keys(),
            application_keypad: false,
        }
    }

    /// Resize the modeled screen without sending anything to the remote peer.
    pub fn resize_local_screen(&self, cols: u16, rows: u16) {
        let mut observer = self.observer.lock().unwrap_or_else(|e| e.into_inner());
        observer.resize_terminal(crate::terminal::TerminalSize {
            cols,
            rows,
            pixel_width: 0,
            pixel_height: 0,
        });
    }
}

fn spawn_authority_transport_acceptor(
    session: Arc<RatatuiRemoteSession>,
    target_id: String,
    session_id: String,
    authority_node_id: String,
) {
    thread::spawn(move || {
        let listener = session
            .listener
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        let Some(listener) = listener else {
            return;
        };
        match listener.accept() {
            Ok((stream, _)) => {
                handle_authority_transport_stream(
                    stream,
                    session.clone(),
                    &target_id,
                    &session_id,
                    &authority_node_id,
                );
            }
            Err(error) => {
                ERROR_LOG.log(format!(
                    "[ratatui-remote-session] authority accept error: {error}"
                ));
            }
        }
    });
}

fn map_agent_session_payload(payload: AgentSessionEntryPayload) -> AgentSessionEntry {
    AgentSessionEntry {
        id: payload.id,
        title: payload.title,
        cwd: payload.cwd,
        updated_at_seconds: payload.updated_at_seconds,
        updated_at_nanos: payload.updated_at_nanos,
    }
}

fn signal_connected(session: &RatatuiRemoteSession, result: Result<(), LifecycleError>) {
    if let Ok(mut guard) = session.connected_tx.lock() {
        if let Some(tx) = guard.take() {
            let _ = tx.send(result);
        }
    }
}

fn handle_authority_transport_stream(
    mut stream: UnixStream,
    session: Arc<RatatuiRemoteSession>,
    target_id: &str,
    _session_id: &str,
    _authority_node_id: &str,
) {
    if let Err(error) = (|| -> Result<(), LifecycleError> {
        let _client_node_id = read_client_hello(&mut stream).map_err(|error| {
            LifecycleError::Io("failed to read authority client hello".to_string(), error)
        })?;
        write_server_hello(&mut stream, "waitagent-ratatui-remote-session").map_err(|error| {
            LifecycleError::Io("failed to write authority server hello".to_string(), error)
        })?;
        Ok(())
    })() {
        ERROR_LOG.log(format!(
            "[ratatui-remote-session] authority handshake failed: {error}"
        ));
        signal_connected(&session, Err(error));
        session.running.store(false, Ordering::Relaxed);
        return;
    }

    signal_connected(&session, Ok(()));
    spawn_remote_task_state_monitor(session.clone());

    {
        let cloned = match stream.try_clone() {
            Ok(cloned) => cloned,
            Err(error) => {
                ERROR_LOG.log(format!(
                    "[ratatui-remote-session] failed to clone authority stream for {target_id}: {error}"
                ));
                session.running.store(false, Ordering::Relaxed);
                return;
            }
        };
        let mut writer_guard = session.writer.lock().unwrap_or_else(|e| e.into_inner());
        *writer_guard = Some(cloned);
    }
    session.flush_open_mirror();
    ERROR_LOG.log(format!(
        "[ratatui-remote-session] authority reader started for {} writer_ready=true",
        target_id
    ));

    let mut output_seq: u64 = 0;
    let mut first_output = true;
    while session.running.load(Ordering::Relaxed) {
        match read_authority_transport_frame(&mut stream) {
            Ok(AuthorityTransportFrame::RawPtyOutput(payload)) => {
                output_seq = output_seq.max(payload.output_seq);
                if first_output {
                    first_output = false;
                    ERROR_LOG.log(format!(
                        "[timing] remote FIRST raw pty output for {target_id} seq={} bytes={}",
                        payload.output_seq,
                        payload.output_bytes.len()
                    ));
                }
                ERROR_LOG.log(format!(
                    "[ratatui-remote-session] received raw pty output for {target_id} seq={} bytes={}",
                    payload.output_seq,
                    payload.output_bytes.len()
                ));
                {
                    let mut observer = session.observer.lock().unwrap_or_else(|e| e.into_inner());
                    observer.feed_raw_output(payload.output_seq, &payload.output_bytes);
                }
                let _ = session
                    .shared
                    .state_sender()
                    .send(StateEvent::RemoteSessionOutput {
                        target_id: target_id.to_string(),
                    });
                signal_output_activity(&session);
            }
            Ok(AuthorityTransportFrame::ControlPlane(envelope)) => match &envelope.payload {
                ControlPlanePayload::OpenMirrorAccepted(_) => {
                    ERROR_LOG.log(format!(
                        "[ratatui-remote-session] mirror opened for {target_id}"
                    ));
                }
                ControlPlanePayload::OpenMirrorRejected(payload) => {
                    ERROR_LOG.log(format!(
                        "[ratatui-remote-session] mirror rejected for {target_id}: {}",
                        payload.message
                    ));
                }
                ControlPlanePayload::ListAgentSessionsResponse(payload) => {
                    let entries = payload
                        .sessions
                        .clone()
                        .into_iter()
                        .map(map_agent_session_payload)
                        .collect();
                    let _ = session.shared.state_sender().send(
                        StateEvent::RemoteAgentSessionsReceived {
                            target_id: target_id.to_string(),
                            request_id: payload.request_id.clone(),
                            result: Ok(entries),
                        },
                    );
                }
                ControlPlanePayload::ListAgentSessionsRejected(payload) => {
                    let _ = session.shared.state_sender().send(
                        StateEvent::RemoteAgentSessionsReceived {
                            target_id: target_id.to_string(),
                            request_id: payload.request_id.clone(),
                            result: Err(payload.reason.clone()),
                        },
                    );
                }
                ControlPlanePayload::RawPtyOutput(payload) => {
                    output_seq = output_seq.max(payload.output_seq);
                    if first_output {
                        first_output = false;
                        ERROR_LOG.log(format!(
                            "[timing] remote FIRST control raw pty output for {target_id} seq={} bytes={}",
                            payload.output_seq,
                            payload.output_bytes.len()
                        ));
                    }
                    ERROR_LOG.log(format!(
                        "[ratatui-remote-session] received control raw pty output for {target_id} seq={} bytes={}",
                        payload.output_seq,
                        payload.output_bytes.len()
                    ));
                    {
                        let mut observer =
                            session.observer.lock().unwrap_or_else(|e| e.into_inner());
                        observer.feed_raw_output(payload.output_seq, &payload.output_bytes);
                    }
                    let _ = session
                        .shared
                        .state_sender()
                        .send(StateEvent::RemoteSessionOutput {
                            target_id: target_id.to_string(),
                        });
                    signal_output_activity(&session);
                }
                _ => {}
            },
            Ok(AuthorityTransportFrame::Ping) => {
                let mut guard = session.writer.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(writer) = guard.as_mut() {
                    let _ = write_authority_transport_frame(writer, &AuthorityTransportFrame::Pong);
                    let _ = writer.flush();
                }
            }
            Ok(AuthorityTransportFrame::Pong) => {}
            Ok(other) => {
                ERROR_LOG.log(format!(
                    "[ratatui-remote-session] unexpected authority frame: {other:?}"
                ));
            }
            Err(error) => {
                ERROR_LOG.log(format!(
                    "[ratatui-remote-session] authority stream read error: {error} for {target_id}"
                ));
                break;
            }
        }
    }
    ERROR_LOG.log(format!(
        "[ratatui-remote-session] authority reader exiting for {target_id}"
    ));
    session.running.store(false, Ordering::Relaxed);
    // Only report a disconnect when the session was not explicitly stopped by
    // StateEventLoop. The loop will decide whether to start a reconnect worker
    // or tear the session down.
    if !session.closed.load(Ordering::SeqCst) {
        let _ = session
            .shared
            .state_sender()
            .send(StateEvent::RemoteSessionDisconnected {
                target_id: target_id.to_string(),
            });
    }
}

fn signal_output_activity(session: &RatatuiRemoteSession) {
    session
        .last_output_ms
        .store(now_millis() as u64, Ordering::Relaxed);
    if session
        .task_state
        .compare_exchange(
            TASK_STATE_INPUT,
            TASK_STATE_RUNNING,
            Ordering::SeqCst,
            Ordering::Relaxed,
        )
        .is_ok()
    {
        let _ = session
            .shared
            .state_sender()
            .send(StateEvent::SessionTaskStateChanged {
                target_id: session.target_id.clone(),
                task_state: crate::domain::session_catalog::ManagedSessionTaskState::Running,
            });
    }
}

fn spawn_remote_task_state_monitor(session: Arc<RatatuiRemoteSession>) {
    thread::spawn(move || {
        while session.running.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(100));
            let last_output = session.last_output_ms.load(Ordering::Relaxed) as u128;
            let idle = now_millis().saturating_sub(last_output);
            if idle >= REMOTE_IDLE_INPUT_MS
                && session
                    .task_state
                    .compare_exchange(
                        TASK_STATE_RUNNING,
                        TASK_STATE_INPUT,
                        Ordering::SeqCst,
                        Ordering::Relaxed,
                    )
                    .is_ok()
            {
                let _ = session
                    .shared
                    .state_sender()
                    .send(StateEvent::SessionTaskStateChanged {
                        target_id: session.target_id.clone(),
                        task_state: crate::domain::session_catalog::ManagedSessionTaskState::Input,
                    });
            }
        }
    });
}

fn now_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn now_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

/// Generate a short, collision-resistant identifier for a single paste file
/// transfer. Falls back to a pid/timestamp string if the OS random source is
/// unavailable; nanosecond precision reduces the chance of collisions when
/// multiple pastes happen in rapid succession.
fn generate_paste_file_id() -> String {
    let mut bytes = [0u8; 12];
    if getrandom::fill(&mut bytes).is_ok() {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    } else {
        format!("{}-{}", std::process::id(), now_nanos())
    }
}

#[cfg(test)]
mod remote_session_tests {
    use super::*;
    use crate::cli::RemoteNetworkConfig;
    use crate::domain::session_catalog::{
        ManagedSessionAddress, ManagedSessionRecord, ManagedSessionTaskState, SessionAvailability,
    };
    use crate::ratatui_node::runtime::SharedState;

    fn test_record(session_id: &str) -> ManagedSessionRecord {
        ManagedSessionRecord {
            address: ManagedSessionAddress::remote_peer(
                "peer#17474".to_string(),
                session_id.to_string(),
            ),
            selector: None,
            availability: SessionAvailability::Online,
            workspace_dir: None,
            workspace_key: None,
            session_role: None,
            opened_by: Vec::new(),
            attached_clients: 0,
            window_count: 1,
            command_name: Some("bash".to_string()),
            display_command_name: None,
            agent_command_name: None,
            current_path: None,
            task_state: ManagedSessionTaskState::Input,
        }
    }

    #[test]
    fn remote_session_opens_mirror() {
        let network = RemoteNetworkConfig::default();
        let shared = SharedState::new(network.clone()).expect("SharedState::new should succeed");
        let record = test_record("sess-opens");
        let socket_name = shared.workspace_id();
        let session = RatatuiRemoteSession::open(&record, &socket_name, &network, &shared, None)
            .expect("open remote session");

        assert_eq!(session.target_id, record.address.qualified_target());
        assert_eq!(session.session_id, record.address.session_id());

        let socket_path =
            authority_transport_socket_path(&socket_name, &session.session_id, &session.target_id);
        assert!(
            socket_path.exists(),
            "authority transport socket should be bound"
        );

        session.stop();
        crate::infra::best_effort::remove_file(&socket_path);
    }

    #[test]
    fn remote_session_feeds_bootstrap_to_observer() {
        let network = RemoteNetworkConfig::default();
        let shared = SharedState::new(network.clone()).expect("SharedState::new should succeed");
        let record = test_record("sess-bootstrap");
        let socket_name = shared.workspace_id();
        let session = RatatuiRemoteSession::open(&record, &socket_name, &network, &shared, None)
            .expect("open remote session");

        {
            let mut observer = session.observer.lock().unwrap_or_else(|e| e.into_inner());
            observer.feed_raw_output(1, b"HELLO WORLD\n");
        }

        let (lines, _styled, _cursor) = session.snapshot();
        assert!(
            lines.iter().any(|line| line.contains("HELLO WORLD")),
            "observer screen should contain bootstrap content, got: {lines:?}"
        );

        let socket_path =
            authority_transport_socket_path(&socket_name, &session.session_id, &session.target_id);
        session.stop();
        crate::infra::best_effort::remove_file(&socket_path);
    }
}
