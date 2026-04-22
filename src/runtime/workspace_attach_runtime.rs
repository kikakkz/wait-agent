use crate::domain::workspace_paths::WorkspacePaths;
use crate::lifecycle::LifecycleError;
use crate::runtime::workspace_daemon_protocol::{read_frame, write_frame, Frame};
use crate::runtime::workspace_readiness::attach_frame_has_visible_workspace;
use crate::terminal::{TerminalRuntime, TerminalSize};
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{self, Sender};
use std::thread;
use std::time::Duration;

const CLIENT_TICK: Duration = Duration::from_millis(50);

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
            tx,
        );

        let mut terminal_runtime = TerminalRuntime::stdio();
        let mut writer = stream;
        let mut stdout = io::stdout().lock();
        let mut first_message_seen = false;
        let mut workspace_visible = false;
        let mut requested_startup_refresh = false;

        loop {
            match rx.recv_timeout(CLIENT_TICK) {
                Ok(AttachClientEvent::Input(bytes)) => {
                    write_frame(&mut writer, &Frame::Input(bytes))?;
                }
                Ok(AttachClientEvent::Socket(frame)) => match frame {
                    Frame::Ack(_) => {
                        first_message_seen = true;
                    }
                    Frame::Snapshot(bytes) | Frame::Output(bytes) => {
                        first_message_seen = true;
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
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if first_message_seen && !workspace_visible && !requested_startup_refresh {
                        request_attach_startup_refresh(&mut writer, self.size)?;
                        requested_startup_refresh = true;
                    }
                    if first_message_seen {
                        if let Some(resized) = terminal_runtime.capture_resize()? {
                            write_frame(&mut writer, &Frame::Resize(resized.into()))?;
                        }
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
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
    Socket(Frame),
    SocketClosed,
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
