use crate::infra::error_log::ERROR_LOG;
use crate::lifecycle::LifecycleError;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Duration;

/// A simple PTY-backed session used when this node hosts a session for a remote
/// viewer. Unlike `RatatuiLocalSession` it captures raw PTY bytes so they can be
/// forwarded over the authority transport.
pub struct RatatuiAuthorityHostSession {
    pub session_id: String,
    pub command_name: String,
    child: Arc<Mutex<Child>>,
    #[allow(dead_code)]
    pty_master: File,
    input_tx: mpsc::Sender<Vec<u8>>,
    output_rx: Mutex<mpsc::Receiver<Vec<u8>>>,
    exit_rx: Mutex<mpsc::Receiver<i32>>,
    shutdown: Arc<AtomicBool>,
}

impl RatatuiAuthorityHostSession {
    /// Spawn a shell in a new PTY with the given initial size.
    pub fn spawn(
        session_id: impl Into<String>,
        command_name: impl Into<String>,
        cols: u16,
        rows: u16,
    ) -> Result<Arc<Self>, LifecycleError> {
        let session_id = session_id.into();
        let command_name = command_name.into();

        let window_size = rustix_openpty::rustix::termios::Winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let pty = rustix_openpty::openpty(None, Some(&window_size)).map_err(|error| {
            LifecycleError::Io(
                "failed to open pty".to_string(),
                io::Error::new(io::ErrorKind::Other, error.to_string()),
            )
        })?;
        let master: OwnedFd = pty.controller;
        let slave: OwnedFd = pty.user;

        // Prefer UTF-8 input handling on the master side.
        if let Ok(termios) = rustix_openpty::rustix::termios::tcgetattr(&master) {
            let mut termios = termios;
            termios.input_modes |=
                rustix_openpty::rustix::termios::InputModes::IUTF8;
            let _ = rustix_openpty::rustix::termios::tcsetattr(
                &master,
                rustix_openpty::rustix::termios::OptionalActions::Now,
                &termios,
            );
        }

        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());
        let mut cmd = Command::new(&shell);
        let master_fd = master.as_raw_fd();
        let slave_fd = slave.as_raw_fd();

        cmd.stdin(
            slave
                .try_clone()
                .map_err(|error| LifecycleError::Io("failed to dup pty slave".to_string(), error))?,
        );
        cmd.stderr(
            slave
                .try_clone()
                .map_err(|error| LifecycleError::Io("failed to dup pty slave".to_string(), error))?,
        );
        cmd.stdout(slave);

        unsafe {
            cmd.pre_exec(move || {
                let _ = libc::setsid();
                let _ = libc::ioctl(slave_fd, libc::TIOCSCTTY, 0);
                libc::close(slave_fd);
                libc::close(master_fd);
                Ok(())
            });
        }

        let child = cmd.spawn().map_err(|error| {
            LifecycleError::Io(
                format!("failed to spawn shell {shell}"),
                error,
            )
        })?;
        let child_arc = Arc::new(Mutex::new(child));

        let mut master_file = File::from(master);
        set_nonblocking(&mut master_file);

        let (output_tx, output_rx) = mpsc::channel::<Vec<u8>>();
        let (input_tx, input_rx) = mpsc::channel::<Vec<u8>>();
        let (exit_tx, exit_rx) = mpsc::channel::<i32>();
        let shutdown = Arc::new(AtomicBool::new(false));

        spawn_pty_reader(
            master_file
                .try_clone()
                .map_err(|error| LifecycleError::Io("failed to clone pty master".to_string(), error))?,
            output_tx,
            shutdown.clone(),
        );
        spawn_pty_writer(
            master_file
                .try_clone()
                .map_err(|error| LifecycleError::Io("failed to clone pty master".to_string(), error))?,
            input_rx,
            shutdown.clone(),
        );
        spawn_child_waiter(child_arc.clone(), exit_tx, shutdown.clone());

        Ok(Arc::new(Self {
            session_id,
            command_name,
            child: child_arc,
            pty_master: master_file,
            input_tx,
            output_rx: Mutex::new(output_rx),
            exit_rx: Mutex::new(exit_rx),
            shutdown,
        }))
    }

    /// Send bytes to the PTY as if typed by the user.
    pub fn feed_input(&self, bytes: Vec<u8>) {
        let _ = self.input_tx.send(bytes);
    }

    /// Resize the PTY.
    pub fn resize(&self, cols: u16, rows: u16) {
        let ws = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        unsafe {
            let _ = libc::ioctl(self.pty_master.as_raw_fd(), libc::TIOCSWINSZ, &ws);
        }
    }

    /// Try to drain raw PTY output bytes produced since the last call.
    pub fn try_recv_output(&self) -> Option<Vec<u8>> {
        let rx = self.output_rx.lock().unwrap();
        match rx.try_recv() {
            Ok(bytes) => Some(bytes),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => None,
        }
    }

    /// Try to receive the shell exit code, if it has exited.
    pub fn try_recv_exit(&self) -> Option<i32> {
        let rx = self.exit_rx.lock().unwrap();
        match rx.try_recv() {
            Ok(status) => Some(status),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => None,
        }
    }

    /// Request a graceful shutdown of the PTY session.
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
        }
    }
}

fn set_nonblocking(file: &mut File) {
    let fd = file.as_raw_fd();
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL, 0);
        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }
}

fn spawn_pty_reader(
    mut master: File,
    output_tx: mpsc::Sender<Vec<u8>>,
    shutdown: Arc<AtomicBool>,
) {
    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            if shutdown.load(Ordering::Relaxed) {
                break;
            }
            match master.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if output_tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => {
                    ERROR_LOG.log(format!(
                        "[ratatui-authority-host] pty reader error: {error}"
                    ));
                    break;
                }
            }
        }
        shutdown.store(true, Ordering::Relaxed);
    });
}

fn spawn_pty_writer(
    mut master: File,
    input_rx: mpsc::Receiver<Vec<u8>>,
    shutdown: Arc<AtomicBool>,
) {
    thread::spawn(move || {
        loop {
            if shutdown.load(Ordering::Relaxed) {
                break;
            }
            match input_rx.recv_timeout(Duration::from_millis(100)) {
                Ok(bytes) => {
                    let mut offset = 0;
                    while offset < bytes.len() {
                        match master.write(&bytes[offset..]) {
                            Ok(n) => offset += n,
                            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                                thread::sleep(Duration::from_millis(5));
                            }
                            Err(error) => {
                                ERROR_LOG.log(format!(
                                    "[ratatui-authority-host] pty writer error: {error}"
                                ));
                                break;
                            }
                        }
                    }
                    let _ = master.flush();
                }
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        shutdown.store(true, Ordering::Relaxed);
    });
}

fn spawn_child_waiter(
    child: Arc<Mutex<Child>>,
    exit_tx: mpsc::Sender<i32>,
    shutdown: Arc<AtomicBool>,
) {
    thread::spawn(move || {
        let status = loop {
            match child.lock() {
                Ok(mut child) => match child.try_wait() {
                    Ok(Some(status)) => break status.code(),
                    Ok(None) => {}
                    Err(error) => {
                        ERROR_LOG.log(format!(
                            "[ratatui-authority-host] child wait error: {error}"
                        ));
                        break None;
                    }
                },
                Err(_) => break None,
            }
            if shutdown.load(Ordering::Relaxed) {
                break None;
            }
            thread::sleep(Duration::from_millis(50));
        };
        let _ = exit_tx.send(status.unwrap_or(-1));
    });
}
