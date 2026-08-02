use crate::domain::session_catalog::{
    ManagedSessionAddress, ManagedSessionRecord, ManagedSessionTaskState, SessionAvailability,
    SessionTransport,
};
use crate::infra::error_log::ERROR_LOG;
use crate::lifecycle::LifecycleError;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::sync::Arc;

use super::authority_host_io_loop::{
    AuthorityHostIoHandle, AuthorityHostIoLoop, AuthorityHostIoRequest,
};
use super::client_writer::{ClientWriterHandle, ClientWriterRequest};
use super::key_translation::{translate_key, KeyTranslationMode};
use super::logical_key::LogicalKey;
use super::reconnect_worker::ReconnectWorker;
use super::runtime::SharedState;
use super::snapshot::{
    build_snapshot, history_response_json, response_json, snapshot_json, ControlResponse,
    HistoryResponse, ServerStatus, SessionView,
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
    authority_host_io_tx: AuthorityHostIoHandle,
    client_writer: ClientWriterHandle,
) -> Result<(), LifecycleError> {
    let mut connected_clients: HashSet<u64> = HashSet::new();
    let mut reconnect_handles: HashMap<String, mpsc::Sender<()>> = HashMap::new();

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
                // If a local TUI was viewing an authority-host session, unregister
                // its "local" console so a remote viewer can take over the PTY size.
                if let Some(target_id) = shared
                    .active_target
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone()
                {
                    if let Some(session_id) =
                        target_id.rsplit_once(':').map(|(_, id)| id.to_string())
                    {
                        let is_host = {
                            let guard = shared
                                .authority_host_sessions
                                .lock()
                                .unwrap_or_else(|e| e.into_inner());
                            guard.contains_key(&session_id)
                        };
                        if is_host {
                            let _ = authority_host_io_tx.send(
                                AuthorityHostIoRequest::UnregisterConsole {
                                    session_id,
                                    console_id: "local".to_string(),
                                },
                            );
                        }
                    }
                }
                broadcast_snapshot(&shared, &client_writer, &connected_clients);
            }

            StateEvent::ClientCommand { client_id, command } => {
                if let ClientCommand::GetHistory { target_id } = &command {
                    let response = build_history_response(&shared, target_id);
                    let payload = history_response_json(&response);
                    client_writer.send(ClientWriterRequest::Write { client_id, payload });
                } else {
                    let outcome = if let ClientCommand::CloseSession { target_id } = &command {
                        if let Some(tx) = reconnect_handles.remove(target_id) {
                            let _ = tx.send(());
                        }
                        shared.handle_session_exit(target_id);
                        broadcast_snapshot(&shared, &client_writer, &connected_clients);
                        CommandOutcome::Message(format!("closed {target_id}"))
                    } else {
                        handle_client_command(
                            &shared,
                            &authority_host_io_tx,
                            &catalog_tx,
                            &client_writer,
                            &connected_clients,
                            command,
                        )
                    };
                    let response: ControlResponse = outcome.into();
                    let payload = response_json(&response);
                    client_writer.send(ClientWriterRequest::Write { client_id, payload });
                }
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

            StateEvent::RemoteSessionOutput { .. } => {
                broadcast_snapshot(&shared, &client_writer, &connected_clients);
            }

            StateEvent::RemoteSessionDisconnected { target_id } => {
                ERROR_LOG.log(format!(
                    "[ratatui-state-loop] remote session disconnected target_id={target_id}"
                ));
                let target_key = target_id.clone();
                match reconnect_handles.entry(target_key) {
                    std::collections::hash_map::Entry::Occupied(_) => {
                        ERROR_LOG.log(format!(
                            "[ratatui-state-loop] reconnect already in progress for {target_id}"
                        ));
                    }
                    std::collections::hash_map::Entry::Vacant(slot) => {
                        // Mark the catalog entry offline so the sidebar shows the
                        // disconnected state immediately.
                        {
                            let mut guard =
                                shared.sessions.lock().unwrap_or_else(|e| e.into_inner());
                            if let Some(record) = guard
                                .values_mut()
                                .find(|r| r.address.qualified_target() == target_id)
                            {
                                record.availability = SessionAvailability::Offline;
                            }
                        }

                        // Stop and discard the old runtime.  The observer screen is
                        // preserved in the catalog record until a reconnect succeeds.
                        {
                            let session = {
                                let guard = shared
                                    .remote_sessions
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner());
                                guard.get(&target_id).cloned()
                            };
                            if let Some(session) = session {
                                session.stop();
                            }
                            let mut guard = shared
                                .remote_sessions
                                .lock()
                                .unwrap_or_else(|e| e.into_inner());
                            guard.remove(&target_id);
                        }

                        // Start a background worker with the last known terminal size.
                        let record = {
                            let guard = shared.sessions.lock().unwrap_or_else(|e| e.into_inner());
                            guard.get(&target_id).cloned()
                        };
                        if let Some(record) = record {
                            let (cols, rows) = shared
                                .last_client_resize
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .unwrap_or((80, 24));
                            let worker = ReconnectWorker::start(
                                record,
                                shared.workspace_id(),
                                shared.network.clone(),
                                cols,
                                rows,
                                shared.clone(),
                                shared.state_sender(),
                            );
                            slot.insert(worker.cancel_tx);
                        }
                        broadcast_snapshot(&shared, &client_writer, &connected_clients);
                    }
                }
            }

            StateEvent::RemoteSessionReconnected { target_id, session } => {
                ERROR_LOG.log(format!(
                    "[ratatui-state-loop] remote session reconnected target_id={target_id}"
                ));
                {
                    let mut guard = shared
                        .remote_sessions
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    guard.insert(target_id.clone(), session.clone());
                }
                {
                    let mut guard = shared.sessions.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(record) = guard
                        .values_mut()
                        .find(|r| r.address.qualified_target() == target_id)
                    {
                        record.availability = SessionAvailability::Online;
                    }
                }
                reconnect_handles.remove(&target_id);
                broadcast_snapshot(&shared, &client_writer, &connected_clients);
            }

            StateEvent::SessionClosed { target_id } => {
                if let Some(tx) = reconnect_handles.remove(&target_id) {
                    let _ = tx.send(());
                }
                shared.handle_session_exit(&target_id);
                broadcast_snapshot(&shared, &client_writer, &connected_clients);
            }

            StateEvent::RemoteNodeOffline { node_id } => {
                ERROR_LOG.log(format!(
                    "[ratatui-state-loop] remote node offline node={node_id}"
                ));
                // Cancel any active reconnect workers for this node before the
                // records are removed.  Failure to cancel would leave a worker
                // running for a session that no longer exists.
                let affected: Vec<String> = {
                    let guard = shared.sessions.lock().unwrap_or_else(|e| e.into_inner());
                    guard
                        .values()
                        .filter(|record| {
                            *record.address.transport() == SessionTransport::RemotePeer
                                && record.address.authority_id() == node_id
                        })
                        .map(|record| record.address.qualified_target())
                        .collect()
                };
                for target_id in affected {
                    if let Some(tx) = reconnect_handles.remove(&target_id) {
                        let _ = tx.send(());
                    }
                }
                shared.remove_remote_sessions_for_node(&node_id);
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
    authority_host_io_tx: &AuthorityHostIoHandle,
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
            let outcome = activate_target(shared, authority_host_io_tx, &target_id);
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
            ERROR_LOG.log(format!(
                "[ratatui-state-loop] resize command cols={cols} rows={rows}"
            ));
            *shared
                .last_client_resize
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some((cols, rows));
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
                            console_id: "local".to_string(),
                            cols,
                            rows,
                        });
                    }
                }
            }
            broadcast_snapshot(shared, client_writer, connected_clients);
            CommandOutcome::Ok
        }

        ClientCommand::Input { target_id, key } => {
            route_input(shared, authority_host_io_tx, &target_id, key);
            CommandOutcome::Ok
        }

        ClientCommand::GetHistory { .. } => {
            // Handled directly in the event-loop dispatcher above.
            CommandOutcome::Ok
        }

        ClientCommand::CreateRemoteSession { authority_node_id } => {
            let outcome = create_remote_session_on_authority(shared, &authority_node_id);
            if matches!(outcome, CommandOutcome::Message(_)) {
                broadcast_snapshot(shared, client_writer, connected_clients);
            }
            outcome
        }

        ClientCommand::CloseSession { .. } => {
            // CloseSession is handled directly in the event-loop dispatcher so
            // it can cancel the active reconnect worker before tearing down state.
            CommandOutcome::Error("CloseSession must be handled by the event loop".to_string())
        }
    }
}

fn build_history_response(shared: &Arc<SharedState>, target_id: &str) -> HistoryResponse {
    let (lines, styled_lines) = shared
        .history_for_target(target_id)
        .unwrap_or_else(|| (Vec::new(), Vec::new()));
    HistoryResponse {
        target_id: target_id.to_string(),
        lines,
        styled_lines,
    }
}

fn create_authority_host_session(
    shared: &Arc<SharedState>,
    authority_host_io_tx: &AuthorityHostIoHandle,
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
    authority_host_io_tx: &AuthorityHostIoHandle,
    target_id: &str,
    key: LogicalKey,
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
            let local_session = {
                let guard = shared
                    .local_sessions
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                guard.get(target_id).cloned()
            };
            if let Some(session) = local_session {
                let mode = local_translation_mode(&session);
                let bytes = translate_key(&key, mode);
                session.feed_input(bytes);
                return;
            }
            // Authority-host sessions are keyed by short session id inside the
            // IO loop. They forward raw PTY bytes to a remote viewer, so we do
            // not have a local terminal mode; translate in normal mode.
            if let Some(session_id) = target_id.rsplit_once(':').map(|(_, id)| id.to_string()) {
                let is_host = {
                    let guard = shared
                        .authority_host_sessions
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    guard.contains_key(&session_id)
                };
                if is_host {
                    let bytes = translate_key(&key, KeyTranslationMode::default());
                    let _ = authority_host_io_tx
                        .send(AuthorityHostIoRequest::WriteInput { session_id, bytes });
                }
            }
        }
        SessionTransport::RemotePeer => {
            let mode = remote_translation_mode(shared, target_id);
            let bytes = translate_key(&key, mode);
            shared.feed_remote_session_input(target_id, bytes);
        }
    }
}

fn local_translation_mode(
    session: &super::local_session::RatatuiLocalSession,
) -> KeyTranslationMode {
    let term = session.term.lock();
    let mode = term.mode();
    KeyTranslationMode {
        application_cursor_keys: mode.contains(alacritty_terminal::term::TermMode::APP_CURSOR),
        application_keypad: mode.contains(alacritty_terminal::term::TermMode::APP_KEYPAD),
    }
}

fn remote_translation_mode(shared: &SharedState, target_id: &str) -> KeyTranslationMode {
    let session = {
        let guard = shared
            .remote_sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        guard.get(target_id).cloned()
    };
    session.map(|s| s.translation_mode()).unwrap_or_default()
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

fn activate_target(
    shared: &Arc<SharedState>,
    authority_host_io_tx: &AuthorityHostIoHandle,
    target_id: &str,
) -> CommandOutcome {
    // We are the single writer of SharedState, so the active target will not
    // change underneath us. Read it first so we can unregister the local TUI
    // console from the previously active authority-host session.
    let previous_target = shared
        .active_target
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();

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

        if let Some(prev) = previous_target.as_deref() {
            if prev != target_id {
                if let Some(session_id) = prev.rsplit_once(':').map(|(_, id)| id.to_string()) {
                    let is_host = {
                        let guard = shared
                            .authority_host_sessions
                            .lock()
                            .unwrap_or_else(|e| e.into_inner());
                        guard.contains_key(&session_id)
                    };
                    if is_host {
                        let _ =
                            authority_host_io_tx.send(AuthorityHostIoRequest::UnregisterConsole {
                                session_id,
                                console_id: "local".to_string(),
                            });
                    }
                }
            }
        }

        *shared
            .active_target
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(target_id.to_string());

        // Register the local TUI console on the newly active authority-host
        // session using the last known main-pane size.
        if let Some(session_id) = target_id.rsplit_once(':').map(|(_, id)| id.to_string()) {
            let is_host = {
                let guard = shared
                    .authority_host_sessions
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                guard.contains_key(&session_id)
            };
            if is_host {
                let (cols, rows) = shared
                    .last_client_resize
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .unwrap_or((80, 24));
                let _ = authority_host_io_tx.send(AuthorityHostIoRequest::Resize {
                    session_id,
                    console_id: "local".to_string(),
                    cols,
                    rows,
                });
            }
        }

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

fn create_remote_session_on_authority(
    shared: &Arc<SharedState>,
    authority_node_id: &str,
) -> CommandOutcome {
    use crate::application::remote_session_creation_service::{
        GrpcRemoteSessionCreationTransport, RemoteSessionCreationTransport,
    };
    use crate::infra::remote_protocol::CreateSessionRequestPayload;
    use std::time::Duration;

    let request_id = format!(
        "ratatui-create-session-{}-{}",
        std::process::id(),
        shared.start_time.elapsed().as_millis()
    );
    let request = CreateSessionRequestPayload {
        request_id: request_id.clone(),
        authority_node_id: authority_node_id.to_string(),
        cwd_hint: None,
        cols: 0,
        rows: 0,
    };

    let result = (|| -> Result<ManagedSessionRecord, LifecycleError> {
        let transport = GrpcRemoteSessionCreationTransport::new(shared.network.clone());
        transport
            .create_session(request, Duration::from_secs(10))
            .map_err(|error| LifecycleError::Protocol(error.to_string()))
            .and_then(|reply| match reply {
                crate::application::remote_session_creation_service::CreateSessionReply::Accepted(
                    accepted,
                ) => {
                    let target = ManagedSessionAddress::remote_peer(
                        authority_node_id.to_string(),
                        accepted.session_id.clone(),
                    )
                    .qualified_target();
                    let record = ManagedSessionRecord {
                        address: ManagedSessionAddress::remote_peer(
                            authority_node_id.to_string(),
                            accepted.session_id.clone(),
                        ),
                        selector: Some(target.clone()),
                        availability: SessionAvailability::Online,
                        workspace_dir: None,
                        workspace_key: Some(accepted.session_id.clone()),
                        session_role: Some(crate::domain::workspace::WorkspaceSessionRole::TargetHost),
                        opened_by: Vec::new(),
                        attached_clients: 0,
                        window_count: 1,
                        command_name: Some("bash".to_string()),
                        display_command_name: None,
                        current_path: None,
                        task_state: ManagedSessionTaskState::Input,
                    };
                    {
                        let mut guard = shared.sessions.lock().unwrap_or_else(|e| e.into_inner());
                        guard.insert(target.clone(), record.clone());
                    }
                    shared.ensure_remote_session(&record).map_err(|error| {
                        LifecycleError::Protocol(format!(
                            "created remote session but failed to open viewer: {error}"
                        ))
                    })?;
                    *shared
                        .active_target
                        .lock()
                        .unwrap_or_else(|e| e.into_inner()) = Some(target.clone());
                    Ok(record)
                }
                crate::application::remote_session_creation_service::CreateSessionReply::Rejected(
                    rejected,
                ) => Err(LifecycleError::Protocol(format!(
                    "remote session creation rejected ({}): {}",
                    rejected.code, rejected.message
                ))),
            })
    })();

    match result {
        Ok(record) => CommandOutcome::Message(format!(
            "created remote session {}",
            record.address.qualified_target()
        )),
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
