use super::snapshot::ControlResponse;
use crate::infra::error_log::ERROR_LOG;
use crate::lifecycle::LifecycleError;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use super::commands::{
    handle_control_command, is_one_shot_control_command, response_should_broadcast,
};
use super::runtime::SharedState;
use super::snapshot::{build_snapshot, response_json};

pub(crate) struct ClientHandle {
    pub(crate) id: u64,
    pub(crate) stream: UnixStream,
    pub(crate) removed: Arc<AtomicBool>,
}

pub(crate) static NEXT_CLIENT_ID: AtomicU64 = AtomicU64::new(1);

pub(crate) fn handle_client(
    mut stream: UnixStream,
    client_id: u64,
    clients: Arc<Mutex<Vec<ClientHandle>>>,
    shared: Arc<SharedState>,
) -> Result<(), LifecycleError> {
    ERROR_LOG.log(format!("[ratatui-node] client {client_id} connected"));
    let removed = Arc::new(AtomicBool::new(false));

    let reader = stream.try_clone().map_err(|error| {
        LifecycleError::Io("failed to clone ratatui client stream".to_string(), error)
    })?;
    let mut reader = BufReader::new(reader);
    let mut line = String::new();

    // Read the first line to decide whether this is a TUI attach or a
    // one-shot control command.
    match reader.read_line(&mut line) {
        Ok(0) | Err(_) => {
            return Ok(());
        }
        Ok(_) => {}
    }

    let trimmed = line.trim();
    ERROR_LOG.log(format!(
        "[ratatui-node] client {client_id} first message: {trimmed}"
    ));

    // One-shot control commands do not join the client list and do not
    // receive the initial snapshot.
    if is_one_shot_control_command(trimmed) {
        let response = handle_control_command(trimmed, &shared, &mut stream)
            .unwrap_or_else(|| ControlResponse::err("unknown command"));
        let _ = writeln!(stream, "{}", response_json(&response));
        let _ = stream.flush();
        return Ok(());
    }

    // Register as a TUI client and send the initial snapshot.
    shared.client_count.fetch_add(1, Ordering::SeqCst);
    if let Ok(clone) = stream.try_clone() {
        let mut guard = clients.lock().unwrap_or_else(|e| e.into_inner());
        guard.push(ClientHandle {
            id: client_id,
            stream: clone,
            removed: removed.clone(),
        });
        drop(guard);
    }

    // The "ATTACH" command is a no-op beyond triggering the snapshot.
    // Build the snapshot without holding the clients lock to avoid a
    // lock-order inversion with the terminal/event-loop threads.
    let count = shared.client_count.load(Ordering::SeqCst);
    let snapshot = build_snapshot(count, &shared);
    let json = super::snapshot::snapshot_json(&snapshot);
    if writeln!(stream, "{json}").is_err() || stream.flush().is_err() {
        remove_client(client_id, &clients, &shared);
        return Ok(());
    }

    let mut forcibly_detached = false;

    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => {
                ERROR_LOG.log(format!("[ratatui-node] client {client_id} disconnected"));
                break;
            }
            Ok(_) => {
                let trimmed = line.trim();
                ERROR_LOG.log(format!(
                    "[ratatui-node] client {client_id} received: {trimmed}"
                ));
                match trimmed {
                    "DETACH" => break,
                    "DETACH_ALL" => {
                        if let Some(response) =
                            handle_control_command(trimmed, &shared, &mut stream)
                        {
                            let _ = writeln!(stream, "{}", response_json(&response));
                            let _ = stream.flush();
                            if response_should_broadcast(&response) {
                                let _ = super::snapshot::broadcast_snapshot(&clients, &shared);
                            }
                        }
                        forcibly_detached = true;
                        break;
                    }
                    _ => {
                        if let Some(response) =
                            handle_control_command(trimmed, &shared, &mut stream)
                        {
                            let _ = writeln!(stream, "{}", response_json(&response));
                            let _ = stream.flush();
                            if response_should_broadcast(&response) {
                                let _ = super::snapshot::broadcast_snapshot(&clients, &shared);
                            }
                        }
                    }
                }
            }
            Err(error) => {
                ERROR_LOG.log(format!(
                    "[ratatui-node] client {client_id} read error: {error:?}"
                ));
                break;
            }
        }
    }

    if !forcibly_detached {
        remove_client(client_id, &clients, &shared);
    }
    Ok(())
}

pub(crate) fn remove_client(
    client_id: u64,
    clients: &Arc<Mutex<Vec<ClientHandle>>>,
    shared: &SharedState,
) {
    let already_removed = {
        let mut guard = clients.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(pos) = guard.iter().position(|handle| handle.id == client_id) {
            let handle = guard.remove(pos);
            handle.removed.store(true, Ordering::SeqCst);
            let _ = handle.stream.shutdown(std::net::Shutdown::Both);
            false
        } else {
            true
        }
    };

    if !already_removed {
        shared.client_count.fetch_sub(1, Ordering::SeqCst);
    }
}
