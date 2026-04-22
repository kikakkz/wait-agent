use crate::config::AppConfig;
use crate::domain::workspace::stable_workspace_key;
use crate::domain::workspace_paths::WorkspacePaths;
use crate::lifecycle::LifecycleError;
use crate::pty::{PtyHandle, PtyManager, PtySize, SpawnRequest, PTY_EOF_ERRNO};
use crate::runtime::workspace_daemon_protocol::{read_frame, write_frame, Frame};
use crate::runtime::workspace_readiness::{
    full_frame_has_chrome, full_frame_has_visible_workspace, looks_like_full_frame,
    workspace_snapshot_ready,
};
use crate::session::SessionAddress;
use crate::terminal::{ScreenSnapshot, TerminalEngine, TerminalSize};
use std::collections::HashMap;
use std::env;
use std::fs::{self, File};
use std::io::{self, Read};
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

const RESET_FRAME_CURSOR: &str = "\x1b[H";

pub struct WorkspaceDaemonRuntime {
    runtime: AppConfig,
    workspace_dir: PathBuf,
    paths: WorkspacePaths,
    pty: PtyHandle,
    engine: TerminalEngine,
    workspace_ready: bool,
    latest_frame_bytes: Option<Vec<u8>>,
    attached_clients: HashMap<u64, AttachedClient>,
    next_client_id: u64,
    rx: Receiver<DaemonEvent>,
    tx: Sender<DaemonEvent>,
    initial_size: TerminalSize,
}

impl WorkspaceDaemonRuntime {
    pub fn start(
        runtime: &AppConfig,
        workspace_dir: PathBuf,
        paths: WorkspacePaths,
        size: TerminalSize,
    ) -> Result<Self, LifecycleError> {
        if let Some(parent) = paths.socket_path.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                LifecycleError::Io(
                    "failed to create waitagent runtime directory".to_string(),
                    error,
                )
            })?;
        }
        if paths.socket_path.exists() {
            let _ = fs::remove_file(&paths.socket_path);
        }

        let listener = UnixListener::bind(&paths.socket_path).map_err(|error| {
            LifecycleError::Io(
                format!(
                    "failed to bind waitagent daemon socket at {}",
                    paths.socket_path.display()
                ),
                error,
            )
        })?;

        let mut pty_manager = PtyManager::new();
        let current_exe = env::current_exe().map_err(|error| {
            LifecycleError::Io("failed to locate current executable".to_string(), error)
        })?;
        let mut args = vec![
            "__workspace-internal".to_string(),
            "--node-id".to_string(),
            runtime.node.node_id.clone(),
        ];
        if let Some(connect) = runtime.network.access_point.as_deref() {
            args.push("--connect".to_string());
            args.push(connect.to_string());
        }

        let pty = pty_manager.spawn(
            SessionAddress::new("local", "workspace-runtime"),
            SpawnRequest {
                program: current_exe.to_string_lossy().into_owned(),
                args,
                size: size.into(),
            },
        )?;

        let engine = TerminalEngine::new(size);
        let (tx, rx) = mpsc::channel();

        spawn_daemon_listener(listener, tx.clone());
        spawn_daemon_pty_reader(
            pty.try_clone_reader().map_err(LifecycleError::Pty)?,
            tx.clone(),
        );

        Ok(Self {
            runtime: runtime.clone(),
            workspace_dir,
            paths,
            pty,
            engine,
            workspace_ready: false,
            latest_frame_bytes: None,
            attached_clients: HashMap::new(),
            next_client_id: 0,
            rx,
            tx,
            initial_size: size,
        })
    }

    pub fn run(mut self) -> Result<(), LifecycleError> {
        let _socket_guard = SocketGuard {
            path: self.paths.socket_path.clone(),
        };

        while let Ok(event) = self.rx.recv() {
            match event {
                DaemonEvent::PtyOutput(bytes) => {
                    self.engine.feed(&bytes);
                    if looks_like_full_frame(&bytes) {
                        let frame_ready = full_frame_has_visible_workspace(&bytes)
                            && full_frame_has_chrome(&bytes);
                        self.workspace_ready |= frame_ready;
                        if frame_ready {
                            self.latest_frame_bytes = Some(bytes.clone());
                        }
                    } else {
                        let snapshot_ready = workspace_snapshot_ready(&self.engine.snapshot());
                        self.workspace_ready |= snapshot_ready;
                    }
                    self.broadcast_frame(Frame::Output(bytes));
                }
                DaemonEvent::PtyClosed => {
                    self.attached_clients.clear();
                    break;
                }
                DaemonEvent::Incoming(stream) => {
                    self.handle_incoming(stream)?;
                }
                DaemonEvent::ClientFrame(client_id, frame) => {
                    if !self.attached_clients.contains_key(&client_id) {
                        continue;
                    }
                    match frame {
                        Frame::Input(bytes) => {
                            self.pty.write_all(&bytes)?;
                        }
                        Frame::Resize(size) => {
                            self.pty.resize(size)?;
                            self.engine.resize(size.into());
                        }
                        Frame::SnapshotRequest => {
                            if let Some(client) = self.attached_clients.get_mut(&client_id) {
                                let snapshot_bytes =
                                    self.latest_frame_bytes.clone().unwrap_or_else(|| {
                                        render_snapshot_bytes(&self.engine.snapshot()).into_bytes()
                                    });
                                write_frame(&mut client.stream, &Frame::Snapshot(snapshot_bytes))?;
                            }
                        }
                        _ => {}
                    }
                }
                DaemonEvent::ClientDisconnected(client_id) => {
                    self.attached_clients.remove(&client_id);
                }
            }
        }

        Ok(())
    }

    fn handle_incoming(&mut self, mut stream: UnixStream) -> Result<(), LifecycleError> {
        let frame = match read_frame(&mut stream) {
            Ok(frame) => frame,
            Err(LifecycleError::Io(_, error))
                if matches!(
                    error.kind(),
                    io::ErrorKind::UnexpectedEof
                        | io::ErrorKind::ConnectionReset
                        | io::ErrorKind::BrokenPipe
                ) =>
            {
                return Ok(());
            }
            Err(error) => return Err(error),
        };

        match frame {
            Frame::Attach(size) => self.attach_client(stream, size),
            Frame::StatusRequest => {
                let response = self.render_status();
                write_frame(&mut stream, &Frame::StatusResponse(response))?;
                Ok(())
            }
            Frame::DetachRequest => {
                let detached = self.detach_all_clients("detached by external request");
                let message = if detached > 0 {
                    format!("detached {detached} attached client(s)")
                } else {
                    "no attached client".to_string()
                };
                write_frame(&mut stream, &Frame::Ack(message))?;
                Ok(())
            }
            other => {
                write_frame(
                    &mut stream,
                    &Frame::Error(format!("unexpected initial daemon frame: {:?}", other)),
                )?;
                Ok(())
            }
        }
    }

    fn attach_client(
        &mut self,
        mut stream: UnixStream,
        size: PtySize,
    ) -> Result<(), LifecycleError> {
        self.pty.resize(size)?;
        self.engine.resize(size.into());

        self.next_client_id += 1;
        let client_id = self.next_client_id;
        write_frame(&mut stream, &Frame::Ack("attached".to_string()))?;
        let snapshot_bytes = self
            .latest_frame_bytes
            .clone()
            .unwrap_or_else(|| render_snapshot_bytes(&self.engine.snapshot()).into_bytes());
        write_frame(&mut stream, &Frame::Snapshot(snapshot_bytes))?;

        let reader = stream.try_clone().map_err(|error| {
            LifecycleError::Io("failed to clone attached client socket".to_string(), error)
        })?;
        spawn_daemon_client_reader(client_id, reader, self.tx.clone());
        self.attached_clients
            .insert(client_id, AttachedClient { stream });
        Ok(())
    }

    fn broadcast_frame(&mut self, frame: Frame) {
        let mut disconnected = Vec::new();
        for (&client_id, client) in &mut self.attached_clients {
            if write_frame(&mut client.stream, &frame).is_err() {
                disconnected.push(client_id);
            }
        }
        for client_id in disconnected {
            self.attached_clients.remove(&client_id);
        }
    }

    fn detach_all_clients(&mut self, reason: &str) -> usize {
        let mut detached = 0;
        for (_, mut client) in self.attached_clients.drain() {
            let _ = write_frame(&mut client.stream, &Frame::Error(reason.to_string()));
            let _ = client.stream.shutdown(Shutdown::Both);
            detached += 1;
        }
        detached
    }

    fn render_status(&self) -> String {
        let snapshot = self.engine.snapshot();
        let child_pid = self
            .pty
            .process_id()
            .map(|pid| pid.to_string())
            .unwrap_or_else(|| "unknown".to_string());
        format!(
            "workspace: {}\nsocket: {}\nkey: {}\nnode: {}\nchild_pid: {}\nready: {}\nattached_clients: {}\nscreen_size: {}x{}\ninitial_size: {}x{}\nalternate_screen: {}",
            self.workspace_dir.display(),
            self.paths.socket_path.display(),
            stable_workspace_key(&self.workspace_dir),
            self.runtime.node.node_id,
            child_pid,
            if self.workspace_ready { "yes" } else { "no" },
            self.attached_clients.len(),
            snapshot.size.rows,
            snapshot.size.cols,
            self.initial_size.rows,
            self.initial_size.cols,
            if snapshot.alternate_screen { "yes" } else { "no" },
        )
    }
}

struct SocketGuard {
    path: PathBuf,
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

struct AttachedClient {
    stream: UnixStream,
}

#[derive(Debug)]
enum DaemonEvent {
    PtyOutput(Vec<u8>),
    PtyClosed,
    Incoming(UnixStream),
    ClientFrame(u64, Frame),
    ClientDisconnected(u64),
}

fn spawn_daemon_listener(listener: UnixListener, tx: Sender<DaemonEvent>) {
    thread::spawn(move || loop {
        match listener.accept() {
            Ok((stream, _)) => {
                if tx.send(DaemonEvent::Incoming(stream)).is_err() {
                    break;
                }
            }
            Err(_) => break,
        }
    });
}

fn spawn_daemon_pty_reader(mut reader: File, tx: Sender<DaemonEvent>) {
    thread::spawn(move || {
        let mut buffer = [0_u8; 4096];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => {
                    let _ = tx.send(DaemonEvent::PtyClosed);
                    break;
                }
                Ok(count) => {
                    if tx
                        .send(DaemonEvent::PtyOutput(buffer[..count].to_vec()))
                        .is_err()
                    {
                        break;
                    }
                }
                Err(error) if error.raw_os_error() == Some(PTY_EOF_ERRNO) => {
                    let _ = tx.send(DaemonEvent::PtyClosed);
                    break;
                }
                Err(_) => {
                    let _ = tx.send(DaemonEvent::PtyClosed);
                    break;
                }
            }
        }
    });
}

fn spawn_daemon_client_reader(client_id: u64, mut stream: UnixStream, tx: Sender<DaemonEvent>) {
    thread::spawn(move || loop {
        match read_frame(&mut stream) {
            Ok(frame) => {
                if tx.send(DaemonEvent::ClientFrame(client_id, frame)).is_err() {
                    break;
                }
            }
            Err(_) => {
                let _ = tx.send(DaemonEvent::ClientDisconnected(client_id));
                break;
            }
        }
    });
}

fn render_snapshot_bytes(snapshot: &ScreenSnapshot) -> String {
    let mut buffer = String::from(RESET_FRAME_CURSOR);
    for (index, line) in snapshot.styled_lines.iter().enumerate() {
        let row = index.saturating_add(1);
        buffer.push_str(&format!("\x1b[{row};1H{line}\x1b[0m\x1b[K"));
    }

    for row in snapshot.styled_lines.len().saturating_add(1)..=snapshot.size.rows as usize {
        buffer.push_str(&format!("\x1b[{row};1H\x1b[K"));
    }

    let cursor_row = snapshot.cursor_row.saturating_add(1);
    let cursor_col = snapshot.cursor_col.saturating_add(1);
    let cursor_visibility = if snapshot.cursor_visible {
        "\x1b[?25h"
    } else {
        "\x1b[?25l"
    };
    let scroll_region = if snapshot.scroll_top == 0
        && snapshot.scroll_bottom.saturating_add(1) == snapshot.size.rows
    {
        "\x1b[r".to_string()
    } else {
        format!(
            "\x1b[{};{}r",
            snapshot.scroll_top.saturating_add(1),
            snapshot.scroll_bottom.saturating_add(1)
        )
    };
    buffer.push_str(&format!(
        "{scroll_region}\x1b[{cursor_row};{cursor_col}H{}{cursor_visibility}",
        snapshot.active_style_ansi
    ));
    buffer
}
