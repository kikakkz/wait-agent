use crate::cli::RemoteNetworkConfig;
use crate::domain::session_catalog::ManagedSessionRecord;
use crate::infra::error_log::ERROR_LOG;
use crate::infra::remote_protocol::{
    ApplyResizePayload, BootstrapMode, ControlPlanePayload, OpenMirrorRequestPayload,
    ProtocolEnvelope, RawPtyInputPayload, REMOTE_PROTOCOL_VERSION,
};
use crate::infra::remote_transport_codec::{
    read_authority_transport_frame, write_authority_transport_frame, AuthorityTransportFrame,
};
use crate::lifecycle::LifecycleError;
use crate::runtime::remote_authority_transport_runtime::authority_transport_socket_path;
use crate::runtime::remote_node::remote_node_ingress_server_runtime::notify_authority_socket_ready;
use crate::runtime::remote_node_transport_runtime::{read_client_hello, write_server_hello};
use crate::runtime::remote_observer_runtime::RemoteObserverRuntime;
use crate::runtime::remote_publication::remote_transport_runtime::LocalNodeMailbox;
use std::fs;
use std::io::{self, Write};
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

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
    socket_path: PathBuf,
    running: Arc<AtomicBool>,
    next_input_seq: AtomicU64,
    initial_cols: Mutex<u16>,
    initial_rows: Mutex<u16>,
}

impl RatatuiRemoteSession {
    /// Start listening on the authority transport socket, register it with the
    /// local ingress owner, and return the session. The acceptor sends the
    /// OpenMirrorRequest once the gRPC bridge connects.
    pub fn open(
        target: &ManagedSessionRecord,
        socket_name: &str,
        network: &RemoteNetworkConfig,
    ) -> Result<Arc<Self>, LifecycleError> {
        let target_id = target.address.qualified_target();
        let session_id = target.address.session_id().to_string();
        let authority_node_id = target.address.authority_id().to_string();
        let socket_path = authority_transport_socket_path(
            socket_name,
            &session_id,
            &target_id,
        );

        if socket_path.exists() {
            let _ = fs::remove_file(&socket_path);
        }
        if let Some(parent) = socket_path.parent() {
            let _ = fs::create_dir_all(parent);
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

        if let Err(error) = notify_authority_socket_ready(network, &authority_node_id, &socket_path) {
            ERROR_LOG.log(format!(
                "[ratatui-remote-session] failed to notify ingress owner for {target_id}: {error}"
            ));
        }

        let observer = RemoteObserverRuntime::new(LocalNodeMailbox::default(), 80, 24);
        let session = Arc::new(Self {
            target_id: target_id.clone(),
            session_id: session_id.clone(),
            authority_node_id: authority_node_id.clone(),
            observer: Mutex::new(observer),
            writer: Mutex::new(None),
            socket_path,
            running: Arc::new(AtomicBool::new(true)),
            next_input_seq: AtomicU64::new(1),
            initial_cols: Mutex::new(80),
            initial_rows: Mutex::new(24),
        });

        spawn_authority_transport_acceptor(
            listener,
            session.clone(),
            target_id,
            session_id,
            authority_node_id,
        );

        Ok(session)
    }

    /// Send an OpenMirrorRequest once the local terminal size is known.
    /// If the ingress bridge has not connected yet, the size is stored and
    /// flushed automatically when the bridge is ready.
    pub fn send_open_mirror(&self, cols: u16, rows: u16) {
        {
            let mut initial_cols = self.initial_cols.lock().unwrap();
            let mut initial_rows = self.initial_rows.lock().unwrap();
            *initial_cols = cols;
            *initial_rows = rows;
        }
        let mut guard = self.writer.lock().unwrap();
        let Some(writer) = guard.as_mut() else {
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
        let envelope = ProtocolEnvelope {
            protocol_version: REMOTE_PROTOCOL_VERSION.to_string(),
            message_id: format!("ratatui-open-mirror-{}", now_millis()),
            message_type: "open_mirror_request",
            timestamp: format!("{}Z", now_millis()),
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
            &AuthorityTransportFrame::ControlPlane(envelope),
        );
        let _ = writer.flush();
    }

    /// Flush a pending OpenMirrorRequest using the last stored size.
    fn flush_open_mirror(&self) {
        let (cols, rows) = {
            let cols = *self.initial_cols.lock().unwrap();
            let rows = *self.initial_rows.lock().unwrap();
            (cols, rows)
        };
        self.send_open_mirror(cols, rows);
    }

    /// Forward keyboard input to the remote session.
    pub fn feed_input(&self, bytes: Vec<u8>) {
        let seq = self.next_input_seq.fetch_add(1, Ordering::Relaxed);
        let mut guard = self.writer.lock().unwrap();
        let Some(writer) = guard.as_mut() else {
            return;
        };
        let payload = RawPtyInputPayload {
            session_id: self.session_id.clone(),
            target_id: self.target_id.clone(),
            attachment_id: format!("ratatui-attach-{}", std::process::id()),
            console_id: format!("ratatui-console-{}", std::process::id()),
            console_host_id: self.authority_node_id.clone(),
            input_seq: seq,
            input_bytes: bytes,
        };
        let frame = AuthorityTransportFrame::RawPtyInput(payload);
        let _ = write_authority_transport_frame(writer, &frame);
        let _ = writer.flush();
    }

    /// Forward a terminal resize to the remote session.
    pub fn resize(&self, cols: u16, rows: u16) {
        let mut guard = self.writer.lock().unwrap();
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
        let envelope = ProtocolEnvelope {
            protocol_version: REMOTE_PROTOCOL_VERSION.to_string(),
            message_id: format!("ratatui-resize-{}", now_millis()),
            message_type: "apply_resize",
            timestamp: format!("{}Z", now_millis()),
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
            &AuthorityTransportFrame::ControlPlane(envelope),
        );
        let _ = writer.flush();
    }

    /// Snapshot the rendered screen as plain text lines and the cursor position.
    pub fn snapshot(&self) -> (Vec<String>, Option<(u16, u16)>) {
        let mut observer = self.observer.lock().unwrap();
        let _ = observer.sync();
        let snap = observer.snapshot();
        let screen = snap.active_screen();
        let cursor = if screen.cursor_row < screen.size.rows && screen.cursor_col < screen.size.cols
        {
            Some((screen.cursor_col, screen.cursor_row))
        } else {
            None
        };
        (screen.lines.clone(), cursor)
    }

    /// Resize the modeled screen without sending anything to the remote peer.
    pub fn resize_local_screen(&self, cols: u16, rows: u16) {
        let mut observer = self.observer.lock().unwrap();
        observer.resize_terminal(crate::terminal::TerminalSize {
            cols,
            rows,
            pixel_width: 0,
            pixel_height: 0,
        });
    }

    pub fn shutdown(&self) {
        self.running.store(false, Ordering::Relaxed);
        let mut guard = self.writer.lock().unwrap();
        if let Some(writer) = guard.take() {
            let _ = writer.shutdown(Shutdown::Both);
        }
        let _ = fs::remove_file(&self.socket_path);
    }
}

fn spawn_authority_transport_acceptor(
    listener: UnixListener,
    session: Arc<RatatuiRemoteSession>,
    target_id: String,
    session_id: String,
    authority_node_id: String,
) {
    thread::spawn(move || {
        let _ = listener.set_nonblocking(true);
        while session.running.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((stream, _)) => {
                    handle_authority_transport_stream(
                        stream,
                        session.clone(),
                        &target_id,
                        &session_id,
                        &authority_node_id,
                    );
                    break;
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => {
                    ERROR_LOG.log(format!(
                        "[ratatui-remote-session] authority accept error: {error}"
                    ));
                    break;
                }
            }
        }
    });
}

fn handle_authority_transport_stream(
    mut stream: UnixStream,
    session: Arc<RatatuiRemoteSession>,
    target_id: &str,
    _session_id: &str,
    _authority_node_id: &str,
) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    if let Err(error) = (|| -> Result<(), LifecycleError> {
        let _client_node_id =
            read_client_hello(&mut stream).map_err(|error| {
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
        session.running.store(false, Ordering::Relaxed);
        return;
    }

    {
        let mut writer_guard = session.writer.lock().unwrap();
        *writer_guard = Some(stream.try_clone().expect("failed to clone authority stream"));
    }
    session.flush_open_mirror();

    let mut output_seq: u64 = 0;
    while session.running.load(Ordering::Relaxed) {
        match read_authority_transport_frame(&mut stream) {
            Ok(AuthorityTransportFrame::RawPtyOutput(payload)) => {
                output_seq = output_seq.max(payload.output_seq);
                let mut observer = session.observer.lock().unwrap();
                observer.feed_raw_output(payload.output_seq, &payload.output_bytes);
            }
            Ok(AuthorityTransportFrame::ControlPlane(envelope)) => {
                match &envelope.payload {
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
                    ControlPlanePayload::RawPtyOutput(payload) => {
                        output_seq = output_seq.max(payload.output_seq);
                        let mut observer = session.observer.lock().unwrap();
                        observer.feed_raw_output(payload.output_seq, &payload.output_bytes);
                    }
                    _ => {}
                }
            }
            Ok(AuthorityTransportFrame::Ping) => {
                let mut guard = session.writer.lock().unwrap();
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
            Err(ref error) if error.is_read_timeout() => {
                thread::sleep(Duration::from_millis(5));
            }
            Err(error) => {
                ERROR_LOG.log(format!(
                    "[ratatui-remote-session] authority stream read error: {error}"
                ));
                break;
            }
        }
    }
    session.running.store(false, Ordering::Relaxed);
}

fn now_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}
