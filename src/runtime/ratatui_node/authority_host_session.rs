use crate::infra::error_log::ERROR_LOG;
use crate::lifecycle::LifecycleError;
use crate::runtime::ratatui_node::authority_host_io_loop::AuthorityHostIoRequest;
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command};
use std::sync::mpsc;

/// A simple PTY-backed session used when this node hosts a session for a remote
/// viewer. Unlike `RatatuiLocalSession` it captures raw PTY bytes so they can be
/// forwarded over the authority transport.
///
/// Phase 1 of the event-driven redesign removes all per-session threads; the
/// PTY master fd and child process are owned by `AuthorityHostIoLoop`.
pub struct RatatuiAuthorityHostSession {
    pub session_id: String,
    #[allow(dead_code)]
    pub command_name: String,
    pub pty_master: File,
    /// The child process is moved to `AuthorityHostIoLoop` when the session is
    /// registered.  After registration this is `None`.
    pub child: Option<Child>,
}

impl RatatuiAuthorityHostSession {
    /// Spawn a shell in a new PTY with the given initial size.
    pub fn spawn(
        session_id: impl Into<String>,
        command_name: impl Into<String>,
        cols: u16,
        rows: u16,
    ) -> Result<Self, LifecycleError> {
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

        // Prefer UTF-8 input handling on the master side. Disable remote echo:
        // the viewing side renders input locally, so the peer PTY must not echo
        // or the screen would show every typed character twice.
        if let Ok(termios) = rustix_openpty::rustix::termios::tcgetattr(&master) {
            let mut termios = termios;
            termios.input_modes |= rustix_openpty::rustix::termios::InputModes::IUTF8;
            let local = &mut termios.local_modes;
            local.remove(rustix_openpty::rustix::termios::LocalModes::ECHO);
            local.remove(rustix_openpty::rustix::termios::LocalModes::ECHOE);
            local.remove(rustix_openpty::rustix::termios::LocalModes::ECHOK);
            local.remove(rustix_openpty::rustix::termios::LocalModes::ECHONL);
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
            slave.try_clone().map_err(|error| {
                LifecycleError::Io("failed to dup pty slave".to_string(), error)
            })?,
        );
        cmd.stderr(
            slave.try_clone().map_err(|error| {
                LifecycleError::Io("failed to dup pty slave".to_string(), error)
            })?,
        );
        cmd.stdout(slave);

        // SAFETY: pre_exec runs in the child process between fork and exec.
        // We only use async-signal-safe libc calls (setsid, ioctl, close).
        unsafe {
            cmd.pre_exec(move || {
                let _ = libc::setsid();
                let _ = libc::ioctl(slave_fd, libc::TIOCSCTTY, 0);
                libc::close(slave_fd);
                libc::close(master_fd);
                Ok(())
            });
        }

        let child = cmd
            .spawn()
            .map_err(|error| LifecycleError::Io(format!("failed to spawn shell {shell}"), error))?;

        let mut master_file = File::from(master);
        set_nonblocking(&mut master_file);

        ERROR_LOG.log(format!(
            "[ratatui-authority-host-session] spawned session={} cols={} rows={}",
            session_id, cols, rows
        ));

        Ok(Self {
            session_id,
            command_name,
            pty_master: master_file,
            child: Some(child),
        })
    }

    /// Send bytes to the PTY as if typed by the user.
    pub fn feed_input(&self, io_tx: &mpsc::Sender<AuthorityHostIoRequest>, bytes: Vec<u8>) {
        let _ = io_tx.send(AuthorityHostIoRequest::WriteInput {
            session_id: self.session_id.clone(),
            bytes,
        });
    }

    /// Resize the PTY.
    pub fn resize(&self, io_tx: &mpsc::Sender<AuthorityHostIoRequest>, cols: u16, rows: u16) {
        let _ = io_tx.send(AuthorityHostIoRequest::Resize {
            session_id: self.session_id.clone(),
            cols,
            rows,
        });
    }

    /// Request a graceful shutdown of the PTY session.
    pub fn shutdown(&mut self, io_tx: &mpsc::Sender<AuthorityHostIoRequest>) {
        let _ = io_tx.send(AuthorityHostIoRequest::UnregisterSession {
            session_id: self.session_id.clone(),
        });
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
        }
    }
}

fn set_nonblocking(file: &mut File) {
    let fd = file.as_raw_fd();
    // SAFETY: fcntl on a valid fd returned by std::fs::File is safe.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL, 0);
        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }
}
