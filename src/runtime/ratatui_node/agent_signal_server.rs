use crate::domain::agent_signal::AgentSignalEnvelope;
use crate::infra::error_log::ERROR_LOG;
use crate::lifecycle::LifecycleError;
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::runtime::SharedState;
use super::state_event::StateEvent;

/// Unix datagram listener for agent lifecycle hooks.
///
/// Agent config files (installed by the hook config services) invoke
/// `waitagent-agent-signal-send <event>` when lifecycle events such as
/// `UserPromptSubmit` or `PermissionRequest` occur.  The sender writes to the
/// socket path exported in the session environment; this server receives the
/// envelope and forwards it to `StateEventLoop` as an `AgentSignalReceived`
/// event.
pub struct AgentSignalServer {
    socket_path: PathBuf,
}

impl AgentSignalServer {
    /// Bind the signal socket for this server and start a background thread
    /// that forwards validated agent signals to `StateEventLoop`.
    pub fn start(shared: Arc<SharedState>) -> Result<Self, LifecycleError> {
        let socket_path = PathBuf::from(&shared.agent_signal_socket_path);
        ensure_socket_parent(&socket_path)?;
        let _ = std::fs::remove_file(&socket_path);
        let socket = UnixDatagram::bind(&socket_path).map_err(|error| {
            LifecycleError::Io(
                format!(
                    "failed to bind agent signal socket {}",
                    socket_path.display()
                ),
                error,
            )
        })?;

        let state_tx = shared.state_sender();
        let token = shared.agent_signal_token.clone();
        std::thread::spawn(move || {
            run_signal_listener(socket, state_tx, token);
        });

        Ok(Self { socket_path })
    }

    /// Remove the socket file on shutdown.
    pub fn cleanup(&self) {
        let _ = std::fs::remove_file(&self.socket_path);
    }
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

fn run_signal_listener(
    socket: UnixDatagram,
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
                ERROR_LOG.log(format!("[agent-signal] recv error: {error}"));
                break;
            }
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

    // In ratatui there is no tmux pane; the session name is the target id.
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
