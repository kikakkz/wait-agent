use crate::domain::session_catalog::{ManagedSessionRecord, SessionTransport};
use crate::infra::error_log::ERROR_LOG;
use crate::lifecycle::LifecycleError;
use std::collections::HashSet;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::sync::Arc;

use super::authority_host_io_loop::{AuthorityHostIoLoop, AuthorityHostIoRequest};
use super::client_writer::{ClientWriterHandle, ClientWriterRequest};
use super::runtime::SharedState;
use super::snapshot::{
    build_snapshot, response_json, snapshot_json, ControlResponse, ServerStatus, SessionView,
};
use super::state_event::{ClientCommand, CommandOutcome, CreatedAuthorityHostTarget, StateEvent};
use crate::runtime::ratatui_remote_connect::connect_remote_host;
use crate::runtime::remote_node_session_sync_runtime::{
    LocalCatalogChangeReason, LocalCatalogChangeRequest,
};

/// Single thread that owns all writes to `SharedState` and all decisions about
/// when to broadcast snapshots.
///
/// Local session lifecycle events, authority-host child exits, session sync
/// creation requests, remote viewer close notifications, and TUI client
/// commands all converge here.  Raw PTY data is forwarded by
/// `AuthorityHostIoLoop` and does not pass through this channel.
pub(crate) struct StateEventLoop {
    tx: mpsc::Sender<StateEvent>,
}

impl StateEventLoop {
    pub(crate) fn start(
        shared: Arc<SharedState>,
        catalog_tx: mpsc::Sender<LocalCatalogChangeRequest>,
        authority_host_io: &AuthorityHostIoLoop,
        client_writer: ClientWriterHandle,
    ) -> Result<Self, LifecycleError> {
        let (tx, rx) = mpsc::channel::<StateEvent>();
        let authority_host_io_tx = authority_host_io.sender();
        std::thread::spawn(move || {
            if let Err(error) =
                run_state_event_loop(shared, rx, catalog_tx, authority_host_io_tx, client_writer)
            {
                ERROR_LOG.log(format!(
                    "[ratatui-state-loop] loop exited with error: {error}"
                ));
            }
        });
        Ok(Self { tx })
    }

    pub(crate) fn sender(&self) -> mpsc::Sender<StateEvent> {
        self.tx.clone()
    }
}

fn run_state_event_loop(
    shared: Arc<SharedState>,
    rx: mpsc::Receiver<StateEvent>,
    catalog_tx: mpsc::Sender<LocalCatalogChangeRequest>,
    authority_host_io_tx: mpsc::Sender<AuthorityHostIoRequest>,
    client_writer: ClientWriterHandle,
) -> Result<(), LifecycleError> {
    let mut connected_clients: HashSet<u64> = HashSet::new();

    while let Ok(event) = rx.recv() {
        match event {
            StateEvent::LocalSessionChildExit { target_id, .. } => {
                ERROR_LOG.log(format!(
                    "[ratatui-state-loop] local session exit target_id={target_id}"
                ));
                // AuthorityHostIoLoop uses short session ids; local sessions are
                // managed by alacritty_terminal and ignore the unregister request.
                if let Some((_, session_id)) = target_id.rsplit_once(':') {
                    let _ = authority_host_io_tx.send(AuthorityHostIoRequest::UnregisterSession {
                        session_id: session_id.to_string(),
                    });
                }
                shared.handle_session_exit(&target_id);
                let _ = catalog_tx.send(LocalCatalogChangeRequest {
                    reason: LocalCatalogChangeReason::LocalTargetExited {
                        target_session_name: target_id,
                    },
                    ack_tx: None,
                });
                broadcast_snapshot(&shared, &client_writer, &connected_clients);
            }

            StateEvent::AuthorityHostSessionChildExited { target_id, .. }
            | StateEvent::AuthorityHostSessionPtyClosed { target_id } => {
                // AuthorityHostIoLoop reports the short session id; turn it into
                // the local qualified target before touching SharedState.
                let qualified_target = format!("{}:{target_id}", shared.local_authority_id());
                ERROR_LOG.log(format!(
                    "[ratatui-state-loop] authority host session exit target_id={qualified_target}"
                ));
                let _ = authority_host_io_tx.send(AuthorityHostIoRequest::UnregisterSession {
                    session_id: target_id,
                });
                shared.handle_session_exit(&qualified_target);
                let _ = catalog_tx.send(LocalCatalogChangeRequest {
                    reason: LocalCatalogChangeReason::LocalTargetExited {
                        target_session_name: qualified_target,
                    },
                    ack_tx: None,
                });
                broadcast_snapshot(&shared, &client_writer, &connected_clients);
            }

            StateEvent::LocalSessionTitleChanged { target_id, title } => {
                shared.set_local_session_title(&target_id, title);
                broadcast_snapshot(&shared, &client_writer, &connected_clients);
            }

            StateEvent::LocalSessionOutput { .. } => {
                broadcast_snapshot(&shared, &client_writer, &connected_clients);
            }

            StateEvent::ClientConnected { client_id } => {
                connected_clients.insert(client_id);
                shared.client_count.fetch_add(1, Ordering::SeqCst);
                // The client's stream is already registered with ClientWriter, so
                // a broadcast will deliver the initial snapshot to the new client
                // and refresh the footer count for everyone else.
                broadcast_snapshot(&shared, &client_writer, &connected_clients);
            }

            StateEvent::ClientDisconnected { client_id } => {
                connected_clients.remove(&client_id);
                shared.client_count.fetch_sub(1, Ordering::SeqCst);
                client_writer.send(ClientWriterRequest::Unregister { client_id });
                broadcast_snapshot(&shared, &client_writer, &connected_clients);
            }

            StateEvent::ClientCommand { client_id, command } => {
                let outcome = handle_client_command(
                    &shared,
                    &authority_host_io_tx,
                    &catalog_tx,
                    &client_writer,
                    &connected_clients,
                    command,
                );
                let response: ControlResponse = outcome.into();
                let payload = response_json(&response);
                client_writer.send(ClientWriterRequest::Write { client_id, payload });
            }

            StateEvent::CreateAuthorityHostSession {
                request_id,
                cols,
                rows,
                reply_tx,
            } => {
                let reply =
                    create_authority_host_session(&shared, &authority_host_io_tx, cols, rows);
                let reply_for_log = reply
                    .as_ref()
                    .map(|created| format!("{}->{}", created.session_id, created.target_id))
                    .unwrap_or_else(|error| format!("error:{error}"));
                ERROR_LOG.log(format!(
                    "[ratatui-state-loop] create authority host session request_id={request_id} result={reply_for_log}"
                ));
                let is_ok = reply.is_ok();
                let _ = reply_tx.send(reply);
                if is_ok {
                    let _ = catalog_tx.send(LocalCatalogChangeRequest {
                        reason: LocalCatalogChangeReason::LocalRuntimeChanged,
                        ack_tx: None,
                    });
                    broadcast_snapshot(&shared, &client_writer, &connected_clients);
                }
            }

            StateEvent::RemoteSessionOutput { .. } | StateEvent::RemoteSessionInputEcho { .. } => {
                broadcast_snapshot(&shared, &client_writer, &connected_clients);
            }

            StateEvent::RemoteSessionClosed { target_id } => {
                shared.handle_session_exit(&target_id);
                broadcast_snapshot(&shared, &client_writer, &connected_clients);
            }
        }
    }
    Ok(())
}

fn broadcast_snapshot(
    shared: &Arc<SharedState>,
    client_writer: &ClientWriterHandle,
    connected_clients: &HashSet<u64>,
) {
    if connected_clients.is_empty() {
        return;
    }
    let count = shared.client_count.load(Ordering::SeqCst);
    let snapshot = build_snapshot(count, shared);
    let payload = snapshot_json(&snapshot);
    client_writer.send(ClientWriterRequest::Broadcast { payload });
}

fn handle_client_command(
    shared: &Arc<SharedState>,
    authority_host_io_tx: &mpsc::Sender<AuthorityHostIoRequest>,
    catalog_tx: &mpsc::Sender<LocalCatalogChangeRequest>,
    client_writer: &ClientWriterHandle,
    connected_clients: &HashSet<u64>,
    command: ClientCommand,
) -> CommandOutcome {
    match command {
        ClientCommand::Attach => CommandOutcome::Ok,

        ClientCommand::Status => {
            let count = shared.client_count.load(Ordering::SeqCst);
            let uptime = shared.start_time.elapsed().as_secs();
            let session_count = shared
                .sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .len();
            let status = ServerStatus {
                port: shared.network.port,
                client_count: count,
                uptime_secs: uptime,
                session_count,
            };
            CommandOutcome::Data(serde_json::to_value(&status).unwrap_or_default())
        }

        ClientCommand::Stop => {
            shared.shutdown.store(true, Ordering::SeqCst);
            let _ = std::os::unix::net::UnixStream::connect(super::socket::ratatui_socket_path(
                shared.network.port,
            ));
            CommandOutcome::Message("stopping".to_string())
        }

        ClientCommand::ListSessions => {
            let guard = shared.sessions.lock().unwrap_or_else(|e| e.into_inner());
            let sessions: Vec<SessionView> = guard.values().map(SessionView::from_record).collect();
            drop(guard);
            CommandOutcome::Data(serde_json::to_value(&sessions).unwrap_or_default())
        }

        ClientCommand::CreateLocalSession => {
            let id = {
                let guard = shared.sessions.lock().unwrap_or_else(|e| e.into_inner());
                format!("{}", guard.len() + 1)
            };
            match shared.create_local_session(&id, 80, 24) {
                Ok(target) => {
                    let _ = catalog_tx.send(LocalCatalogChangeRequest {
                        reason: LocalCatalogChangeReason::LocalRuntimeChanged,
                        ack_tx: None,
                    });
                    broadcast_snapshot(shared, client_writer, connected_clients);
                    CommandOutcome::Message(format!("created local session {target}"))
                }
                Err(error) => CommandOutcome::Error(error.to_string()),
            }
        }

        ClientCommand::ActivateTarget { target_id } => {
            let outcome = activate_target(shared, &target_id);
            if matches!(outcome, CommandOutcome::Ok) {
                broadcast_snapshot(shared, client_writer, connected_clients);
            }
            outcome
        }

        ClientCommand::ConnectRemoteHost { profile_name } => {
            let outcome = connect_remote_host_target(shared, &profile_name);
            if matches!(outcome, CommandOutcome::Message(_)) {
                broadcast_snapshot(shared, client_writer, connected_clients);
            }
            outcome
        }

        ClientCommand::DetachAll => {
            shared.detach_all_clients();
            // Ask the client writer to unregister every known client.
            for client_id in connected_clients.iter() {
                client_writer.send(ClientWriterRequest::Unregister {
                    client_id: *client_id,
                });
            }
            CommandOutcome::Ok
        }

        ClientCommand::Resize { cols, rows } => {
            let transport = {
                let guard = shared.sessions.lock().unwrap_or_else(|e| e.into_inner());
                let active = shared
                    .active_target
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                active.and_then(|target| {
                    guard
                        .values()
                        .find(|s| s.address.qualified_target() == target)
                        .map(|s| s.address.transport().clone())
                })
            };
            match transport {
                Some(SessionTransport::RemotePeer) => {
                    shared.resize_active_remote_session(cols, rows);
                }
                _ => {
                    shared.resize_active_local_session(cols, rows);
                    if let Some(id) = active_authority_host_session_id(shared) {
                        let _ = authority_host_io_tx.send(AuthorityHostIoRequest::Resize {
                            session_id: id,
                            cols,
                            rows,
                        });
                    }
                }
            }
            broadcast_snapshot(shared, client_writer, connected_clients);
            CommandOutcome::Ok
        }

        ClientCommand::Input { target_id, bytes } => {
            route_input(shared, authority_host_io_tx, &target_id, bytes);
            CommandOutcome::Ok
        }
    }
}

fn create_authority_host_session(
    shared: &Arc<SharedState>,
    authority_host_io_tx: &mpsc::Sender<AuthorityHostIoRequest>,
    cols: u16,
    rows: u16,
) -> Result<CreatedAuthorityHostTarget, LifecycleError> {
    let (session_id, mut session, target_id) = shared.create_authority_host_session(cols, rows)?;
    let pty_master = session.pty_master.try_clone().map_err(|error| {
        LifecycleError::Io(
            "failed to clone authority host pty master".to_string(),
            error,
        )
    })?;
    let child = session.child.take().ok_or_else(|| {
        LifecycleError::Protocol("authority host session missing child process".to_string())
    })?;
    let _ = authority_host_io_tx.send(AuthorityHostIoRequest::RegisterSession {
        session_id: session_id.clone(),
        pty_master,
        child,
        output_tx: None,
    });
    {
        let mut host_guard = shared
            .authority_host_sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        host_guard.insert(session_id.clone(), Arc::new(session));
    }
    Ok(CreatedAuthorityHostTarget {
        session_id,
        target_id,
    })
}

fn route_input(
    shared: &SharedState,
    authority_host_io_tx: &mpsc::Sender<AuthorityHostIoRequest>,
    target_id: &str,
    bytes: Vec<u8>,
) {
    let record = {
        let guard = shared.sessions.lock().unwrap_or_else(|e| e.into_inner());
        guard.get(target_id).cloned()
    };
    let Some(record) = record else {
        return;
    };
    match record.address.transport() {
        SessionTransport::Local => {
            // Local PTY sessions are keyed by qualified target.
            if shared
                .local_sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains_key(target_id)
            {
                shared.feed_local_session_input(target_id, bytes);
                return;
            }
            // Authority-host sessions are keyed by short session id inside the
            // IO loop.
            if let Some(session_id) = target_id.rsplit_once(':').map(|(_, id)| id.to_string()) {
                if shared
                    .authority_host_sessions
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .contains_key(&session_id)
                {
                    let _ = authority_host_io_tx
                        .send(AuthorityHostIoRequest::WriteInput { session_id, bytes });
                }
            }
        }
        SessionTransport::RemotePeer => {
            shared.feed_remote_session_input(target_id, bytes);
        }
    }
}

fn active_authority_host_session_id(shared: &SharedState) -> Option<String> {
    // Lock order: sessions -> active_target -> authority_host_sessions.
    let guard = shared.sessions.lock().unwrap_or_else(|e| e.into_inner());
    let active = shared
        .active_target
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()?;
    let record = guard
        .values()
        .find(|r| r.address.qualified_target() == active)?;
    let session_id = record.address.session_id().to_string();
    let is_host = shared
        .authority_host_sessions
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains_key(&session_id);
    if is_host {
        Some(session_id)
    } else {
        None
    }
}

fn activate_target(shared: &Arc<SharedState>, target_id: &str) -> CommandOutcome {
    let record = {
        let guard = shared.sessions.lock().unwrap_or_else(|e| e.into_inner());
        guard
            .values()
            .find(|s| s.address.qualified_target() == target_id)
            .cloned()
    };
    if let Some(record) = record {
        if *record.address.transport() == SessionTransport::RemotePeer {
            if let Err(error) = shared.ensure_remote_session(&record) {
                return CommandOutcome::Error(error.to_string());
            }
        }
        *shared
            .active_target
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(target_id.to_string());
        CommandOutcome::Ok
    } else {
        CommandOutcome::Error("unknown target".to_string())
    }
}

fn connect_remote_host_target(shared: &Arc<SharedState>, profile_name: &str) -> CommandOutcome {
    let sessions_vec: Vec<ManagedSessionRecord> = {
        let guard = shared.sessions.lock().unwrap_or_else(|e| e.into_inner());
        guard.values().cloned().collect()
    };
    let sessions_arc = Arc::new(std::sync::Mutex::new(sessions_vec));
    match connect_remote_host(profile_name, &sessions_arc, &shared.network) {
        Ok(record) => {
            let target = record.address.qualified_target();
            {
                let mut guard = shared.sessions.lock().unwrap_or_else(|e| e.into_inner());
                guard.retain(|_, session| session.address.id() != record.address.id());
                guard.insert(target.clone(), record.clone());
            }
            if let Err(error) = shared.ensure_remote_session(&record) {
                return CommandOutcome::Error(error.to_string());
            }
            *shared
                .active_target
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(target.clone());
            CommandOutcome::Message(format!("connected {target}"))
        }
        Err(error) => CommandOutcome::Error(error.to_string()),
    }
}

impl From<CommandOutcome> for ControlResponse {
    fn from(outcome: CommandOutcome) -> Self {
        match outcome {
            CommandOutcome::Ok => ControlResponse::ok(),
            CommandOutcome::Message(message) => ControlResponse::ok_message(message),
            CommandOutcome::Error(message) => ControlResponse::err(message),
            CommandOutcome::Data(data) => ControlResponse::ok_data(data),
        }
    }
}
