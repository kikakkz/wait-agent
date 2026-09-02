use crate::infra::error_log::ERROR_LOG;
use crate::lifecycle::LifecycleError;
use crate::ratatui_node::agent_signal_env::AgentSignalEnv;
use crate::ratatui_node::authority_host_io_loop::{
    AuthorityHostIoHandle, AuthorityHostIoRequest, SessionChild,
};
#[cfg(unix)]
use std::fs::File;

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
    #[cfg(unix)]
    pub pty_master: File,
    /// ConPTY on Windows. Moved into `AuthorityHostIoLoop` on registration;
    /// always `None` while the session is registered.
    #[cfg(windows)]
    pub conpty: Option<crate::platform::pty::ConPty>,
    /// The child process is moved to `AuthorityHostIoLoop` when the session is
    /// registered.  After registration this is `None`.
    pub child: Option<SessionChild>,
}

impl RatatuiAuthorityHostSession {
    /// Spawn a shell in a new PTY with the given initial size.
    #[cfg(unix)]
    pub fn spawn(
        session_id: impl Into<String>,
        command_name: impl Into<String>,
        cols: u16,
        rows: u16,
        signal_env: AgentSignalEnv,
    ) -> Result<Self, LifecycleError> {
        use std::os::fd::AsRawFd;
        use std::os::unix::process::CommandExt;
        use std::process::Command;

        let session_id = session_id.into();
        let command_name = command_name.into();

        // Leave the PTY in its default termios for shell startup.  Bash will
        // switch the slave into the raw/readline mode it needs once it has
        // finished initialization.  We only make the master non-blocking so
        // the IO loop can poll it; the kernel line discipline is otherwise
        // left alone.
        let crate::platform::pty::PtyPair { mut master, slave } =
            crate::platform::pty::openpty(cols, rows)
                .map_err(|error| LifecycleError::Io("failed to open pty".to_string(), error))?;

        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());
        let mut cmd = Command::new(&shell);
        cmd.env(
            "TERM",
            std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".to_string()),
        );
        cmd.env("COLORTERM", "truecolor");
        signal_env.apply_to_command(&mut cmd)?;
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
                let _ = libc::ioctl(slave_fd, libc::TIOCSCTTY as libc::c_ulong, 0);
                libc::close(slave_fd);
                libc::close(master_fd);
                Ok(())
            });
        }

        let child = cmd
            .spawn()
            .map_err(|error| LifecycleError::Io(format!("failed to spawn shell {shell}"), error))?;

        crate::platform::pty::set_nonblocking(&mut master).ok();

        ERROR_LOG.log(format!(
            "[ratatui-authority-host-session] spawned session={session_id} cols={cols} rows={rows}"
        ));

        Ok(Self {
            session_id,
            command_name,
            pty_master: master,
            child: Some(child),
        })
    }

    /// Spawn a shell in a new ConPTY with the given initial size.
    ///
    /// Stable Rust's `std::process::Command` cannot express the
    /// `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE` attribute, so the actual
    /// `CreateProcessW` call lives in `platform::pty`.
    #[cfg(windows)]
    pub fn spawn(
        session_id: impl Into<String>,
        command_name: impl Into<String>,
        cols: u16,
        rows: u16,
        signal_env: AgentSignalEnv,
    ) -> Result<Self, LifecycleError> {
        use std::collections::HashMap;

        let session_id = session_id.into();
        let command_name = command_name.into();

        let mut conpty = crate::platform::pty::openpty(cols, rows)
            .map_err(|error| LifecycleError::Io("failed to create ConPTY".to_string(), error))?;

        let shell = super::local_session::default_shell();
        // Skip non-Unicode variables instead of panicking like `std::env::vars`.
        let mut env: HashMap<String, String> = std::env::vars_os()
            .filter_map(
                |(key, value)| match (key.into_string(), value.into_string()) {
                    (Ok(key), Ok(value)) => Some((key, value)),
                    _ => None,
                },
            )
            .collect();
        signal_env.apply_to_hashmap(&mut env)?;
        let child =
            crate::platform::pty::spawn_shell(std::ffi::OsStr::new(&shell), &env, &mut conpty)
                .map_err(|error| {
                    LifecycleError::Io(format!("failed to spawn shell {shell}"), error)
                })?;

        ERROR_LOG.log(format!(
            "[ratatui-authority-host-session] spawned session={} cols={} rows={}",
            session_id, cols, rows
        ));

        Ok(Self {
            session_id,
            command_name,
            conpty: Some(conpty),
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

    /// Ask the IO loop to send a bootstrap ANSI snapshot of the current terminal
    /// state to the active output sender.
    ///
    /// Called when a remote viewer opens (or reopens) its mirror so the viewer
    /// immediately sees the current screen and scrollback history instead of a
    /// blank pane.
    pub fn send_bootstrap(&self, io_tx: &AuthorityHostIoHandle) {
        let _ = io_tx.send(AuthorityHostIoRequest::SendBootstrap {
            session_id: self.session_id.clone(),
        });
    }
}
