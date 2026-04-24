use crate::domain::workspace_paths::WorkspacePaths;
use crate::lifecycle::LifecycleError;
use crate::runtime::workspace_daemon_protocol::{read_frame, write_frame, Frame};
use crate::runtime::workspace_readiness::attach_frame_has_visible_workspace;
use crate::terminal::{TerminalRuntime, TerminalSize};
use std::io::{self, Read, Write};
use std::os::raw::{c_int, c_void};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::mpsc::{self, Sender};
use std::thread;

const SIGWINCH: c_int = 28;

static ATTACH_SIGWINCH_WRITE_FD: AtomicI32 = AtomicI32::new(-1);

extern "C" {
    fn signal(signum: c_int, handler: extern "C" fn(c_int)) -> usize;
    fn write(fd: c_int, buf: *const c_void, count: usize) -> isize;
}

pub struct WorkspaceAttachRuntime {
    paths: WorkspacePaths,
    size: TerminalSize,
}

impl WorkspaceAttachRuntime {
    pub fn new(paths: WorkspacePaths, size: TerminalSize) -> Self {
        Self { paths, size }
    }

    pub fn run(self) -> Result<(), LifecycleError> {
        let mut stream = UnixStream::connect(&self.paths.socket_path).map_err(|error| {
            LifecycleError::Io(
                format!(
                    "failed to connect to waitagent daemon at {}",
                    self.paths.socket_path.display()
                ),
                error,
            )
        })?;
        write_frame(&mut stream, &Frame::Attach(self.size.into()))?;

        let terminal = TerminalRuntime::stdio();
        let _alternate_screen = terminal.enter_alternate_screen()?;
        let _raw_mode = terminal.enter_raw_mode()?;

        let (tx, rx) = mpsc::channel();
        spawn_attach_stdin_reader(tx.clone());
        spawn_attach_socket_reader(
            stream.try_clone().map_err(|error| {
                LifecycleError::Io("failed to clone daemon socket".to_string(), error)
            })?,
            tx.clone(),
        );
        let _resize_watcher = spawn_attach_resize_watcher(tx.clone()).map_err(|error| {
            LifecycleError::Io("failed to install attach resize watcher".to_string(), error)
        })?;

        let mut writer = stream;
        let mut stdout = io::stdout().lock();
        let mut workspace_visible = false;
        let mut requested_startup_refresh = false;

        loop {
            match rx.recv() {
                Ok(AttachClientEvent::Input(bytes)) => {
                    write_frame(&mut writer, &Frame::Input(bytes))?;
                }
                Ok(AttachClientEvent::Resize(size)) => {
                    write_frame(&mut writer, &Frame::Resize(size.into()))?;
                }
                Ok(AttachClientEvent::Socket(frame)) => match frame {
                    Frame::Ack(_) => {}
                    Frame::Snapshot(bytes) | Frame::Output(bytes) => {
                        if !workspace_visible {
                            if !attach_frame_has_visible_workspace(&bytes) {
                                if !requested_startup_refresh {
                                    request_attach_startup_refresh(&mut writer, self.size)?;
                                    requested_startup_refresh = true;
                                }
                                continue;
                            }
                            workspace_visible = true;
                        }
                        stdout.write_all(&bytes).map_err(|error| {
                            LifecycleError::Io("failed to write attach output".to_string(), error)
                        })?;
                        stdout.flush().map_err(|error| {
                            LifecycleError::Io("failed to flush attach output".to_string(), error)
                        })?;
                    }
                    Frame::Error(message) => {
                        return Err(LifecycleError::Protocol(message));
                    }
                    Frame::StatusResponse(_) => {}
                    _ => {
                        return Err(LifecycleError::Protocol(format!(
                            "unexpected attach frame: {:?}",
                            frame
                        )));
                    }
                },
                Ok(AttachClientEvent::SocketClosed) => break,
                Err(_) => break,
            }
        }

        Ok(())
    }
}

fn request_attach_startup_refresh(
    writer: &mut UnixStream,
    size: TerminalSize,
) -> Result<(), LifecycleError> {
    let mut bumped = size;
    if bumped.rows < u16::MAX {
        bumped.rows += 1;
    } else if bumped.cols < u16::MAX {
        bumped.cols += 1;
    }
    write_frame(writer, &Frame::Resize(bumped.into()))?;
    write_frame(writer, &Frame::Resize(size.into()))?;
    write_frame(writer, &Frame::SnapshotRequest)
}

#[derive(Debug)]
enum AttachClientEvent {
    Input(Vec<u8>),
    Resize(TerminalSize),
    Socket(Frame),
    SocketClosed,
}

struct AttachResizeWatcher {
    _writer: UnixStream,
}

impl Drop for AttachResizeWatcher {
    fn drop(&mut self) {
        ATTACH_SIGWINCH_WRITE_FD.store(-1, Ordering::Relaxed);
    }
}

fn spawn_attach_stdin_reader(tx: Sender<AttachClientEvent>) {
    thread::spawn(move || {
        let stdin = io::stdin();
        let mut handle = stdin.lock();
        let mut buffer = [0_u8; 4096];
        loop {
            match handle.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => {
                    if tx
                        .send(AttachClientEvent::Input(buffer[..count].to_vec()))
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

fn spawn_attach_socket_reader(mut stream: UnixStream, tx: Sender<AttachClientEvent>) {
    thread::spawn(move || loop {
        match read_frame(&mut stream) {
            Ok(frame) => {
                if tx.send(AttachClientEvent::Socket(frame)).is_err() {
                    break;
                }
            }
            Err(_) => {
                let _ = tx.send(AttachClientEvent::SocketClosed);
                break;
            }
        }
    });
}

fn spawn_attach_resize_watcher(tx: Sender<AttachClientEvent>) -> io::Result<AttachResizeWatcher> {
    let (mut reader, writer) = UnixStream::pair()?;
    ATTACH_SIGWINCH_WRITE_FD.store(writer.as_raw_fd(), Ordering::Relaxed);
    unsafe {
        signal(SIGWINCH, attach_sigwinch_handler);
    }

    thread::spawn(move || {
        let terminal = TerminalRuntime::stdio();
        let mut buffer = [0_u8; 64];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(_) => {
                    if tx
                        .send(AttachClientEvent::Resize(
                            terminal.current_size_or_default(),
                        ))
                        .is_err()
                    {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    Ok(AttachResizeWatcher { _writer: writer })
}

extern "C" fn attach_sigwinch_handler(_signal: c_int) {
    let fd = ATTACH_SIGWINCH_WRITE_FD.load(Ordering::Relaxed);
    if fd < 0 {
        return;
    }

    let byte = 1_u8;
    unsafe {
        let _ = write(fd, (&byte as *const u8).cast::<c_void>(), 1);
    }
}
