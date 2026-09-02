//! Cross-platform agent lifecycle signal channel.

use crate::domain::agent_signal::AgentSignalEnvelope;
use crate::infra::error_log::ERROR_LOG;
use crate::lifecycle::LifecycleError;
use crate::ratatui_node::state_event::StateEvent;
use std::path::Path;

/// Return the platform-specific endpoint where the agent signal server listens.
///
/// On Unix this is a filesystem path for a Unix datagram socket.
/// On Windows this is a named pipe path like `\\.\pipe\waitagent-signal-<port>`.
pub fn default_signal_endpoint(port: u16) -> String {
    #[cfg(unix)]
    {
        crate::ratatui_node::socket::ratatui_socket_dir()
            .join(format!("signal-{port}.sock"))
            .to_string_lossy()
            .to_string()
    }
    #[cfg(windows)]
    {
        format!(r"\\.\pipe\waitagent-signal-{port}")
    }
}

/// Server-side listener for agent lifecycle signals.
pub struct AgentSignalServer {
    #[cfg(unix)]
    socket_path: std::path::PathBuf,
    #[cfg(windows)]
    _phantom: (),
}

impl AgentSignalServer {
    /// Start the signal server for this node.
    #[cfg(unix)]
    pub fn start(
        socket_path: &std::path::Path,
        token: &str,
        state_tx: std::sync::mpsc::Sender<StateEvent>,
    ) -> Result<Self, LifecycleError> {
        use std::os::unix::net::UnixDatagram;

        let socket_path = socket_path.to_path_buf();
        ensure_socket_parent(&socket_path)?;
        crate::infra::best_effort::remove_file(&socket_path);
        let socket = UnixDatagram::bind(&socket_path).map_err(|error| {
            LifecycleError::Io(
                format!(
                    "failed to bind agent signal socket {}",
                    socket_path.display()
                ),
                error,
            )
        })?;

        let token = token.to_string();
        std::thread::spawn(move || {
            run_unix_signal_listener(socket, state_tx, token);
        });

        Ok(Self { socket_path })
    }

    /// Windows named-pipe signal server.
    #[cfg(windows)]
    pub fn start(
        socket_path: &std::path::Path,
        token: &str,
        state_tx: std::sync::mpsc::Sender<StateEvent>,
    ) -> Result<Self, LifecycleError> {
        let endpoint = socket_path.to_string_lossy().to_string();
        let token = token.to_string();
        std::thread::spawn(move || {
            run_windows_signal_listener(&endpoint, state_tx, token);
        });
        Ok(Self { _phantom: () })
    }

    /// Remove any runtime artifacts on shutdown.
    #[cfg(unix)]
    pub fn cleanup(&self) {
        crate::infra::best_effort::remove_file(&self.socket_path);
    }

    #[cfg(windows)]
    pub fn cleanup(&self) {}
}

#[cfg(unix)]
fn run_unix_signal_listener(
    socket: std::os::unix::net::UnixDatagram,
    state_tx: std::sync::mpsc::Sender<StateEvent>,
    token: String,
) {
    let mut buf = vec![0u8; 4096];
    loop {
        match socket.recv_from(&mut buf) {
            Ok((len, _addr)) => {
                let bytes = &buf[..len];
                handle_signal_bytes(bytes, &state_tx, &token);
            }
            Err(error) => {
                ERROR_LOG.log_error(format!("[agent-signal] recv error: {error}"));
                break;
            }
        }
    }
}

#[cfg(windows)]
fn run_windows_signal_listener(
    endpoint: &str,
    state_tx: std::sync::mpsc::Sender<StateEvent>,
    token: String,
) {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateNamedPipeA, PIPE_ACCESS_DUPLEX, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE,
        PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
    };
    use windows_sys::Win32::System::Pipes::{ConnectNamedPipe, DisconnectNamedPipe};

    let endpoint_c = std::ffi::CString::new(endpoint).unwrap_or_default();
    if endpoint_c.is_empty() {
        ERROR_LOG.log("[agent-signal] invalid named pipe endpoint".to_string());
        return;
    }

    loop {
        // SAFETY: `endpoint_c` is a valid null-terminated string. Security
        // attributes are NULL (default). The pipe is byte-stream, blocking,
        // duplex, local-only.
        let pipe = unsafe {
            CreateNamedPipeA(
                endpoint_c.as_ptr(),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE | PIPE_WAIT,
                PIPE_UNLIMITED_INSTANCES,
                4096,
                4096,
                0,
                std::ptr::null_mut(),
            )
        };
        if pipe == INVALID_HANDLE_VALUE {
            ERROR_LOG.log_error(format!(
                "[agent-signal] CreateNamedPipeA failed: {}",
                std::io::Error::last_os_error()
            ));
            break;
        }

        // SAFETY: `pipe` is a valid named pipe handle returned by CreateNamedPipeA.
        let connected = unsafe { ConnectNamedPipe(pipe, std::ptr::null_mut()) } != 0
            || std::io::Error::last_os_error().raw_os_error()
                == Some(windows_sys::Win32::System::Pipes::ERROR_PIPE_CONNECTED as i32);

        if connected {
            let mut buf = [0u8; 4096];
            let mut read = 0u32;
            // SAFETY: `pipe` is valid and `buf` outlives the call.
            let read_ok = unsafe {
                windows_sys::Win32::Storage::FileSystem::ReadFile(
                    pipe,
                    buf.as_mut_ptr(),
                    buf.len() as u32,
                    &mut read,
                    std::ptr::null_mut(),
                )
            } != 0;
            if read_ok && read > 0 {
                handle_signal_bytes(&buf[..read as usize], &state_tx, &token);
            }
        }

        // SAFETY: `pipe` is still valid until we close it below.
        unsafe {
            let _ = DisconnectNamedPipe(pipe);
            CloseHandle(pipe);
        }
    }
}

fn handle_signal_bytes(bytes: &[u8], state_tx: &std::sync::mpsc::Sender<StateEvent>, token: &str) {
    let envelope: AgentSignalEnvelope = match serde_json::from_slice(bytes) {
        Ok(envelope) => envelope,
        Err(error) => {
            ERROR_LOG.log(format!(
                "[agent-signal] malformed envelope: {error}: {}",
                String::from_utf8_lossy(bytes)
            ));
            return;
        }
    };

    if envelope.token != token {
        ERROR_LOG.log(format!(
            "[agent-signal] rejected envelope with bad token from agent={} event={}",
            envelope.agent, envelope.event
        ));
        return;
    }

    let target_id = if envelope.session.is_empty() {
        envelope.pane.clone()
    } else {
        envelope.session.clone()
    };

    if target_id.is_empty() {
        ERROR_LOG.log(format!(
            "[agent-signal] envelope missing session/pane: agent={} event={}",
            envelope.agent, envelope.event
        ));
        return;
    }

    let _ = state_tx.send(StateEvent::AgentSignalReceived {
        target_id,
        agent: envelope.agent,
        event: envelope.event,
        payload: envelope.payload,
    });
}

fn ensure_socket_parent(socket_path: &Path) -> Result<(), LifecycleError> {
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            LifecycleError::Io(
                format!(
                    "failed to create agent signal socket parent {}",
                    parent.display()
                ),
                error,
            )
        })?;
    }
    Ok(())
}

/// Send an agent signal envelope. Used by the bundled sender binary.
///
/// `target` is a UDS path on Unix and a named pipe path on Windows.
#[cfg(unix)]
#[allow(dead_code)]
pub fn send_agent_signal(target: &str, envelope: &AgentSignalEnvelope) -> std::io::Result<()> {
    use std::os::unix::net::UnixDatagram;

    let socket_path = std::path::Path::new(target);
    let bytes = serde_json::to_vec(envelope)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let socket = UnixDatagram::unbound()?;
    socket.send_to(&bytes, socket_path)?;
    Ok(())
}

#[cfg(windows)]
pub fn send_agent_signal(target: &str, envelope: &AgentSignalEnvelope) -> std::io::Result<()> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileA, FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_READ, FILE_GENERIC_WRITE, OPEN_EXISTING,
    };

    let target_c = std::ffi::CString::new(target)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid target"))?;
    let bytes = serde_json::to_vec(envelope)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

    // SAFETY: `target_c` is a valid null-terminated pipe name.
    let handle = unsafe {
        CreateFileA(
            target_c.as_ptr(),
            FILE_GENERIC_READ | FILE_GENERIC_WRITE,
            0,
            std::ptr::null_mut(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }

    let mut written = 0u32;
    // SAFETY: `handle` is valid and `bytes` outlives the call.
    let write_ok = unsafe {
        windows_sys::Win32::Storage::FileSystem::WriteFile(
            handle,
            bytes.as_ptr(),
            bytes.len() as u32,
            &mut written,
            std::ptr::null_mut(),
        )
    } != 0;

    // SAFETY: `handle` was created above and is no longer used after this.
    unsafe {
        CloseHandle(handle);
    }

    if !write_ok {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}
