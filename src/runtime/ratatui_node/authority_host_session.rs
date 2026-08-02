use crate::infra::error_log::ERROR_LOG;
use crate::lifecycle::LifecycleError;
use crate::runtime::ratatui_node::authority_host_io_loop::{
    AuthorityHostIoHandle, AuthorityHostIoRequest,
};
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command};

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

        // Leave the PTY in its default termios for shell startup.  Bash will
        // switch the slave into the raw/readline mode it needs once it has
        // finished initialization.  We only make the master non-blocking so
        // the IO loop can poll it; the kernel line discipline is otherwise
        // left alone.

        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());
        let mut cmd = Command::new(&shell);
        cmd.env(
            "TERM",
            std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".to_string()),
        );
        cmd.env("COLORTERM", "truecolor");
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
    pub fn feed_input(&self, io_tx: &AuthorityHostIoHandle, bytes: impl Into<Vec<u8>>) {
        let _ = io_tx.send(AuthorityHostIoRequest::WriteInput {
            session_id: self.session_id.clone(),
            bytes: bytes.into(),
        });
    }

    /// Resize the PTY on behalf of a specific console.
    ///
    /// The IO loop uses the console id to decide whether this console is the
    /// active console for the session; only the active console's dimensions are
    /// applied to the PTY.
    pub fn resize_for_console(
        &self,
        io_tx: &AuthorityHostIoHandle,
        cols: u16,
        rows: u16,
        console_id: impl Into<String>,
    ) {
        let _ = io_tx.send(AuthorityHostIoRequest::Resize {
            session_id: self.session_id.clone(),
            cols,
            rows,
            console_id: console_id.into(),
        });
    }

    /// Remove a console from the session.
    ///
    /// Called when a remote viewer closes its mirror or the local TUI detaches.
    /// The IO loop elects a new active console and resizes the PTY if needed.
    pub fn unregister_console(&self, io_tx: &AuthorityHostIoHandle, console_id: impl Into<String>) {
        let _ = io_tx.send(AuthorityHostIoRequest::UnregisterConsole {
            session_id: self.session_id.clone(),
            console_id: console_id.into(),
        });
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
