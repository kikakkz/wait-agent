use crate::domain::agent_detector::DetectorRegistry;
use crate::domain::agent_signal::AgentStateEffect;
use crate::domain::session_catalog::{
    ManagedSessionTaskState, SessionAvailability, SessionTransport,
};
use crate::infra::error_log::ERROR_LOG;
use crate::infra::settings_store::SettingsStore;
use crate::lifecycle::LifecycleError;
use crate::ports::session_creation::RemoteSessionCreationRequest;
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
use crate::host::ssh::remote_host_connect_runtime::{
    RemoteHostConnectRequest, RemoteHostConnectRuntime, SshRemotePortProbeFactory,
};
use crate::host::ssh::remote_host_history_store::RemoteHostHistoryStore;
use crate::host::ssh::ssh_remote_host_bootstrapper::SshRemoteHostBootstrapper;
use crate::remote::node::remote_node_session_sync_runtime::{
    LocalCatalogChangeReason, LocalCatalogChangeRequest,
};
use crate::remote::node::remote_runtime_owner_runtime::RemoteRuntimeOwnerRuntime;

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
        remote_owner: RemoteRuntimeOwnerRuntime,
        settings_store: SettingsStore,
    ) -> Result<Self, LifecycleError> {
        let (tx, rx) = mpsc::channel::<StateEvent>();
        let authority_host_io_tx = authority_host_io.sender();
        std::thread::spawn(move || {
            if let Err(error) = run_state_event_loop(
                shared,
                rx,
                catalog_tx,
                authority_host_io_tx,
                client_writer,
                remote_owner,
                settings_store,
            ) {
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
    remote_owner: RemoteRuntimeOwnerRuntime,
    settings_store: SettingsStore,
) -> Result<(), LifecycleError> {
    let mut connected_clients: HashSet<u64> = HashSet::new();
    let mut reconnect_handles: HashMap<String, mpsc::Sender<()>> = HashMap::new();

    while let Ok(event) = rx.recv() {
        match event {
            StateEvent::LocalSessionChildExit { target_id, .. } => {
                handle_local_session_child_exit(
                    &shared,
                    &authority_host_io_tx,
                    &catalog_tx,
                    &client_writer,
                    &connected_clients,
                    target_id,
                );
            }

            StateEvent::AuthorityHostSessionChildExited { target_id, .. }
            | StateEvent::AuthorityHostSessionPtyClosed { target_id } => {
                handle_authority_host_session_exit(
                    &shared,
                    &authority_host_io_tx,
                    &catalog_tx,
                    &client_writer,
                    &connected_clients,
                    target_id,
                );
            }

            StateEvent::LocalSessionTitleChanged { target_id, title } => {
                shared.set_local_session_title(&target_id, title);
                broadcast_snapshot(&shared, &client_writer, &connected_clients);
            }

            StateEvent::SessionTaskStateChanged {
                target_id,
                task_state,
            } => {
                shared.set_session_task_state(&target_id, task_state);
                broadcast_snapshot(&shared, &client_writer, &connected_clients);
            }

            StateEvent::SessionCommandNameChanged {
                target_id,
                command_name,
            } => {
                shared.set_session_command_name(&target_id, command_name);
                broadcast_snapshot(&shared, &client_writer, &connected_clients);
            }

            StateEvent::LocalSessionOutput { .. } => {
                broadcast_snapshot(&shared, &client_writer, &connected_clients);
            }

            StateEvent::ClientConnected { client_id } => {
                handle_client_connected(&shared, &client_writer, &mut connected_clients, client_id);
            }

            StateEvent::ClientDisconnected { client_id } => {
                handle_client_disconnected(
                    &shared,
                    &authority_host_io_tx,
                    &client_writer,
                    &mut connected_clients,
                    client_id,
                );
            }

            StateEvent::ClientCommand { client_id, command } => {
                handle_client_command_event(
                    &shared,
                    &remote_owner,
                    &authority_host_io_tx,
                    &catalog_tx,
                    &client_writer,
                    &connected_clients,
                    &mut reconnect_handles,
                    client_id,
                    command,
                    &settings_store,
                );
            }

            StateEvent::CreateAuthorityHostSession {
                request_id,
                cols,
                rows,
                reply_tx,
            } => {
                handle_create_authority_host_session(
                    &shared,
                    &authority_host_io_tx,
                    &catalog_tx,
                    &client_writer,
                    &connected_clients,
                    request_id,
                    cols,
                    rows,
                    reply_tx,
                );
            }

            StateEvent::RemoteSessionOutput { .. } => {
                broadcast_snapshot(&shared, &client_writer, &connected_clients);
            }

            StateEvent::RemoteSessionDisconnected { target_id } => {
                handle_remote_session_disconnected(
                    &shared,
                    &client_writer,
                    &connected_clients,
                    &mut reconnect_handles,
                    target_id,
                );
            }

            StateEvent::RemoteSessionReconnected { target_id, session } => {
                handle_remote_session_reconnected(
                    &shared,
                    &client_writer,
                    &connected_clients,
                    &mut reconnect_handles,
                    target_id,
                    session,
                );
            }

            StateEvent::AgentSignalReceived {
                target_id,
                agent,
                event,
                payload,
            } => {
                handle_agent_signal_received(
                    &shared,
                    &client_writer,
                    &connected_clients,
                    target_id,
                    agent,
                    event,
                    payload,
                );
            }

            StateEvent::RemoteSessionCatalogUpdated { record } => {
                shared.update_remote_session_record(record);
                broadcast_snapshot(&shared, &client_writer, &connected_clients);
            }

            StateEvent::SessionClosed { target_id } => {
                handle_session_closed(
                    &shared,
                    &client_writer,
                    &connected_clients,
                    &mut reconnect_handles,
                    target_id,
                );
            }

            StateEvent::RemoteNodeOffline { node_id } => {
                handle_remote_node_offline(
                    &shared,
                    &client_writer,
                    &connected_clients,
                    &mut reconnect_handles,
                    node_id,
                );
            }
        }
    }
    Ok(())
}

fn handle_local_session_child_exit(
    shared: &Arc<SharedState>,
    authority_host_io_tx: &AuthorityHostIoHandle,
    catalog_tx: &mpsc::Sender<LocalCatalogChangeRequest>,
    client_writer: &ClientWriterHandle,
    connected_clients: &HashSet<u64>,
    target_id: String,
) {
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
    broadcast_snapshot(shared, client_writer, connected_clients);
}

fn handle_authority_host_session_exit(
    shared: &Arc<SharedState>,
    authority_host_io_tx: &AuthorityHostIoHandle,
    catalog_tx: &mpsc::Sender<LocalCatalogChangeRequest>,
    client_writer: &ClientWriterHandle,
    connected_clients: &HashSet<u64>,
    target_id: String,
) {
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
    broadcast_snapshot(shared, client_writer, connected_clients);
}

fn handle_client_connected(
    shared: &Arc<SharedState>,
    client_writer: &ClientWriterHandle,
    connected_clients: &mut HashSet<u64>,
    client_id: u64,
) {
    connected_clients.insert(client_id);
    shared.clients.client_count.fetch_add(1, Ordering::SeqCst);
    // The client's stream is already registered with ClientWriter, so
    // a broadcast will deliver the initial snapshot to the new client
    // and refresh the footer count for everyone else.
    broadcast_snapshot(shared, client_writer, connected_clients);
}

fn handle_client_disconnected(
    shared: &Arc<SharedState>,
    authority_host_io_tx: &AuthorityHostIoHandle,
    client_writer: &ClientWriterHandle,
    connected_clients: &mut HashSet<u64>,
    client_id: u64,
) {
    connected_clients.remove(&client_id);
    shared.clients.client_count.fetch_sub(1, Ordering::SeqCst);
    client_writer.send(ClientWriterRequest::Unregister { client_id });
    // If a local TUI was viewing an authority-host session, unregister
    // its "local" console so a remote viewer can take over the PTY size.
    if let Some(target_id) = shared
        .sessions
        .active_target
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
    {
        if let Some(session_id) = target_id.rsplit_once(':').map(|(_, id)| id.to_string()) {
            let is_host = {
                let guard = shared
                    .sessions
                    .authority_host_sessions
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                guard.contains_key(&session_id)
            };
            if is_host {
                let _ = authority_host_io_tx.send(AuthorityHostIoRequest::UnregisterConsole {
                    session_id,
                    console_id: "local".to_string(),
                });
            }
        }
    }
    broadcast_snapshot(shared, client_writer, connected_clients);
}

#[allow(clippy::too_many_arguments)]
fn handle_client_command_event(
    shared: &Arc<SharedState>,
    remote_owner: &RemoteRuntimeOwnerRuntime,
    authority_host_io_tx: &AuthorityHostIoHandle,
    catalog_tx: &mpsc::Sender<LocalCatalogChangeRequest>,
    client_writer: &ClientWriterHandle,
    connected_clients: &HashSet<u64>,
    reconnect_handles: &mut HashMap<String, mpsc::Sender<()>>,
    client_id: u64,
    command: ClientCommand,
    settings_store: &SettingsStore,
) {
    if let ClientCommand::GetHistory { target_id } = &command {
        let response = build_history_response(shared, target_id);
        let payload = history_response_json(&response);
        client_writer.send(ClientWriterRequest::Write { client_id, payload });
        return;
    }

    let outcome = if let ClientCommand::CloseSession { target_id } = &command {
        if let Some(tx) = reconnect_handles.remove(target_id) {
            let _ = tx.send(());
        }
        shared.handle_session_exit(target_id);
        broadcast_snapshot(shared, client_writer, connected_clients);
        CommandOutcome::Message(format!("closed {target_id}"))
    } else {
        handle_client_command(
            shared,
            remote_owner,
            authority_host_io_tx,
            catalog_tx,
            client_writer,
            connected_clients,
            command,
            settings_store,
        )
    };
    let response: ControlResponse = outcome.into();
    let payload = response_json(&response);
    client_writer.send(ClientWriterRequest::Write { client_id, payload });
}

#[allow(clippy::too_many_arguments)]
fn handle_create_authority_host_session(
    shared: &Arc<SharedState>,
    authority_host_io_tx: &AuthorityHostIoHandle,
    catalog_tx: &mpsc::Sender<LocalCatalogChangeRequest>,
    client_writer: &ClientWriterHandle,
    connected_clients: &HashSet<u64>,
    request_id: String,
    cols: u16,
    rows: u16,
    reply_tx: std::sync::mpsc::Sender<
        Result<CreatedAuthorityHostTarget, crate::lifecycle::LifecycleError>,
    >,
) {
    let reply = create_authority_host_session(shared, authority_host_io_tx, cols, rows);
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
        broadcast_snapshot(shared, client_writer, connected_clients);
    }
}

fn handle_remote_session_disconnected(
    shared: &Arc<SharedState>,
    client_writer: &ClientWriterHandle,
    connected_clients: &HashSet<u64>,
    reconnect_handles: &mut HashMap<String, mpsc::Sender<()>>,
    target_id: String,
) {
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
                let mut guard = shared
                    .sessions
                    .sessions
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
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
                        .sessions
                        .remote_sessions
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    guard.get(&target_id).cloned()
                };
                if let Some(session) = session {
                    session.stop();
                }
                let mut guard = shared
                    .sessions
                    .remote_sessions
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                guard.remove(&target_id);
            }

            // Start a background worker with the last known terminal size.
            let record = {
                let guard = shared
                    .sessions
                    .sessions
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                guard.get(&target_id).cloned()
            };
            if let Some(record) = record {
                let (cols, rows) = shared
                    .resize
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
            broadcast_snapshot(shared, client_writer, connected_clients);
        }
    }
}

fn handle_remote_session_reconnected(
    shared: &Arc<SharedState>,
    client_writer: &ClientWriterHandle,
    connected_clients: &HashSet<u64>,
    reconnect_handles: &mut HashMap<String, mpsc::Sender<()>>,
    target_id: String,
    session: Arc<super::remote_session::RatatuiRemoteSession>,
) {
    ERROR_LOG.log(format!(
        "[ratatui-state-loop] remote session reconnected target_id={target_id}"
    ));
    {
        let mut guard = shared
            .sessions
            .remote_sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        guard.insert(target_id.clone(), session.clone());
    }
    {
        let mut guard = shared
            .sessions
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(record) = guard
            .values_mut()
            .find(|r| r.address.qualified_target() == target_id)
        {
            record.availability = SessionAvailability::Online;
        }
    }
    reconnect_handles.remove(&target_id);
    broadcast_snapshot(shared, client_writer, connected_clients);
}

fn handle_agent_signal_received(
    shared: &Arc<SharedState>,
    client_writer: &ClientWriterHandle,
    connected_clients: &HashSet<u64>,
    target_id: String,
    agent: String,
    event: String,
    payload: serde_json::Value,
) {
    ERROR_LOG.log(format!(
        "[ratatui-state-loop] agent signal target_id={target_id} agent={agent} event={event}"
    ));
    if event == "cwd" {
        if let Some(path) = payload.as_str() {
            shared.set_session_current_path(&target_id, std::path::PathBuf::from(path));
        }
    } else {
        let effect = DetectorRegistry::default().signal_state_effect(&agent, &event, &payload);
        match effect {
            Some(AgentStateEffect::Set(state)) => {
                shared.set_session_task_state(&target_id, state);
                shared.set_session_command_name(&target_id, agent);
            }
            Some(AgentStateEffect::Clear) => {
                shared.set_session_task_state(&target_id, ManagedSessionTaskState::Input);
                shared.clear_session_command_name(&target_id);
            }
            None => {}
        }
    }
    broadcast_snapshot(shared, client_writer, connected_clients);
}

fn handle_session_closed(
    shared: &Arc<SharedState>,
    client_writer: &ClientWriterHandle,
    connected_clients: &HashSet<u64>,
    reconnect_handles: &mut HashMap<String, mpsc::Sender<()>>,
    target_id: String,
) {
    if let Some(tx) = reconnect_handles.remove(&target_id) {
        let _ = tx.send(());
    }
    shared.handle_session_exit(&target_id);
    broadcast_snapshot(shared, client_writer, connected_clients);
}

fn handle_remote_node_offline(
    shared: &Arc<SharedState>,
    client_writer: &ClientWriterHandle,
    connected_clients: &HashSet<u64>,
    reconnect_handles: &mut HashMap<String, mpsc::Sender<()>>,
    node_id: String,
) {
    ERROR_LOG.log(format!(
        "[ratatui-state-loop] remote node offline node={node_id}"
    ));
    // Cancel any active reconnect workers for this node before the
    // records are removed.  Failure to cancel would leave a worker
    // running for a session that no longer exists.
    let affected: Vec<String> = {
        let guard = shared
            .sessions
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner());
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
    broadcast_snapshot(shared, client_writer, connected_clients);
}

fn broadcast_snapshot(
    shared: &Arc<SharedState>,
    client_writer: &ClientWriterHandle,
    connected_clients: &HashSet<u64>,
) {
    if connected_clients.is_empty() {
        return;
    }
    let count = shared.clients.client_count.load(Ordering::SeqCst);
    let snapshot = build_snapshot(count, shared);
    let payload = snapshot_json(&snapshot);
    #[cfg(test)]
    {
        use std::sync::atomic::Ordering;
        TEST_BROADCAST_COUNT.fetch_add(1, Ordering::SeqCst);
        if let Ok(mut guard) = TEST_BROADCAST_THREAD_ID.lock() {
            *guard = Some(std::thread::current().id());
        }
    }
    client_writer.send(ClientWriterRequest::Broadcast { payload });
}

#[cfg(test)]
static TEST_BROADCAST_THREAD_ID: std::sync::Mutex<Option<std::thread::ThreadId>> =
    std::sync::Mutex::new(None);
#[cfg(test)]
static TEST_BROADCAST_COUNT: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

#[allow(clippy::too_many_arguments)]
fn handle_client_command(
    shared: &Arc<SharedState>,
    remote_owner: &RemoteRuntimeOwnerRuntime,
    authority_host_io_tx: &AuthorityHostIoHandle,
    catalog_tx: &mpsc::Sender<LocalCatalogChangeRequest>,
    client_writer: &ClientWriterHandle,
    connected_clients: &HashSet<u64>,
    command: ClientCommand,
    settings_store: &SettingsStore,
) -> CommandOutcome {
    match command {
        ClientCommand::Attach => CommandOutcome::Ok,

        ClientCommand::Status => {
            let count = shared.clients.client_count.load(Ordering::SeqCst);
            let uptime = shared.start_time.elapsed().as_secs();
            let session_count = shared
                .sessions
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
            let guard = shared
                .sessions
                .sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let sessions: Vec<SessionView> = guard.values().map(SessionView::from_record).collect();
            drop(guard);
            CommandOutcome::Data(serde_json::to_value(&sessions).unwrap_or_default())
        }

        ClientCommand::CreateLocalSession => {
            let id = {
                let guard = shared
                    .sessions
                    .sessions
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
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
            let outcome = connect_remote_host_target(shared, remote_owner, &profile_name);
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
                .resize
                .last_client_resize
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some((cols, rows));
            let transport = {
                let guard = shared
                    .sessions
                    .sessions
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                let active = shared
                    .sessions
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

        ClientCommand::PasteText { target_id, text } => {
            route_paste_text(shared, authority_host_io_tx, &target_id, text);
            CommandOutcome::Ok
        }

        ClientCommand::PasteFile {
            target_id,
            filename_hint,
            bytes,
        } => handle_paste_file(
            shared,
            authority_host_io_tx,
            &target_id,
            &filename_hint,
            bytes,
        ),

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

        ClientCommand::SetPublic { endpoint, save } => {
            shared.set_public_endpoint_override(endpoint.clone());
            match endpoint {
                Some(ref endpoint) => {
                    let _ = settings_store.set_public(endpoint, save);
                }
                None => {
                    let _ = settings_store.clear_public();
                }
            }
            broadcast_snapshot(shared, client_writer, connected_clients);
            match endpoint {
                Some(endpoint) => {
                    CommandOutcome::Message(format!("public endpoint set to {endpoint}"))
                }
                None => CommandOutcome::Message("public endpoint cleared".to_string()),
            }
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
        cols,
        rows,
    });
    {
        let mut host_guard = shared
            .sessions
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

fn route_paste_text(
    shared: &SharedState,
    authority_host_io_tx: &AuthorityHostIoHandle,
    target_id: &str,
    text: String,
) {
    let record = {
        let guard = shared
            .sessions
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        guard.get(target_id).cloned()
    };
    let Some(record) = record else {
        return;
    };
    let bytes = text.into_bytes();
    match record.address.transport() {
        SessionTransport::Local => {
            let local_session = {
                let guard = shared
                    .sessions
                    .local_sessions
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                guard.get(target_id).cloned()
            };
            if let Some(session) = local_session {
                session.feed_input(bytes);
                return;
            }
            if let Some(session_id) = target_id.rsplit_once(':').map(|(_, id)| id.to_string()) {
                let is_host = {
                    let guard = shared
                        .sessions
                        .authority_host_sessions
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    guard.contains_key(&session_id)
                };
                if is_host {
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

fn handle_paste_file(
    shared: &SharedState,
    _authority_host_io_tx: &AuthorityHostIoHandle,
    target_id: &str,
    filename_hint: &str,
    bytes: Vec<u8>,
) -> CommandOutcome {
    let record = {
        let guard = shared
            .sessions
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        guard.get(target_id).cloned()
    };
    let Some(record) = record else {
        return CommandOutcome::Error("unknown target".to_string());
    };

    match record.address.transport() {
        SessionTransport::Local => {
            let cached_path =
                match super::clipboard_cache::write_clipboard_file(filename_hint, &bytes) {
                    Ok(path) => path,
                    Err(error) => {
                        return CommandOutcome::Error(format!(
                            "failed to cache pasted file: {error}"
                        ))
                    }
                };

            let local_session = {
                let guard = shared
                    .sessions
                    .local_sessions
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                guard.get(target_id).cloned()
            };
            if let Some(session) = local_session {
                let path_string = cached_path.to_string_lossy().into_owned();
                session.feed_input(path_string.into_bytes());
                return CommandOutcome::Ok;
            }

            if let Some(session_id) = target_id.rsplit_once(':').map(|(_, id)| id.to_string()) {
                let is_host = {
                    let guard = shared
                        .sessions
                        .authority_host_sessions
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    guard.contains_key(&session_id)
                };
                if is_host {
                    let path_string = cached_path.to_string_lossy().into_owned();
                    let _ = _authority_host_io_tx.send(AuthorityHostIoRequest::WriteInput {
                        session_id,
                        bytes: path_string.into_bytes(),
                    });
                    return CommandOutcome::Ok;
                }
            }

            CommandOutcome::Error("local session not found".to_string())
        }
        SessionTransport::RemotePeer => {
            // Remote peer file paste is implemented in Phase 4 by forwarding the
            // bytes over the gRPC bridge and caching them on the remote node.
            CommandOutcome::Error("remote file paste is not yet implemented (Phase 4)".to_string())
        }
    }
}

fn route_input(
    shared: &SharedState,
    authority_host_io_tx: &AuthorityHostIoHandle,
    target_id: &str,
    key: LogicalKey,
) {
    let record = {
        let guard = shared
            .sessions
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner());
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
                    .sessions
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
                        .sessions
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
            .sessions
            .remote_sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        guard.get(target_id).cloned()
    };
    session.map(|s| s.translation_mode()).unwrap_or_default()
}

fn active_authority_host_session_id(shared: &SharedState) -> Option<String> {
    // Lock order: sessions -> active_target -> authority_host_sessions.
    let guard = shared
        .sessions
        .sessions
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let active = shared
        .sessions
        .active_target
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()?;
    let record = guard
        .values()
        .find(|r| r.address.qualified_target() == active)?;
    let session_id = record.address.session_id().to_string();
    let is_host = shared
        .sessions
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
    ERROR_LOG.log(format!(
        "[timing] activate_target START target_id={target_id}"
    ));
    // We are the single writer of SharedState, so the active target will not
    // change underneath us. Read it first so we can unregister the local TUI
    // console from the previously active authority-host session.
    let previous_target = shared
        .sessions
        .active_target
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();

    let record = {
        let guard = shared
            .sessions
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        guard
            .values()
            .find(|s| s.address.qualified_target() == target_id)
            .cloned()
    };

    if let Some(record) = record {
        if *record.address.transport() == SessionTransport::RemotePeer {
            ERROR_LOG.log(format!(
                "[timing] activate_target ensure_remote_session target_id={target_id}"
            ));
            if let Err(error) = shared.ensure_remote_session(&record) {
                return CommandOutcome::Error(error.to_string());
            }
            ERROR_LOG.log(format!(
                "[timing] activate_target ensure_remote_session DONE target_id={target_id}"
            ));
        }

        if let Some(prev) = previous_target.as_deref() {
            if prev != target_id {
                if let Some(session_id) = prev.rsplit_once(':').map(|(_, id)| id.to_string()) {
                    let is_host = {
                        let guard = shared
                            .sessions
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
            .sessions
            .active_target
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(target_id.to_string());

        // Open the remote mirror immediately using the last known main-pane
        // size so the first rendered frame has the correct dimensions.
        if *record.address.transport() == SessionTransport::RemotePeer {
            let (cols, rows) = shared
                .resize
                .last_client_resize
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .unwrap_or((80, 24));
            ERROR_LOG.log(format!(
                "[timing] activate_target resize_active_remote_session target_id={target_id} cols={cols} rows={rows}"
            ));
            shared.resize_active_remote_session(cols, rows);
            ERROR_LOG.log(format!(
                "[timing] activate_target resize_active_remote_session DONE target_id={target_id}"
            ));
        }

        // Register the local TUI console on the newly active authority-host
        // session using the last known main-pane size.
        if let Some(session_id) = target_id.rsplit_once(':').map(|(_, id)| id.to_string()) {
            let is_host = {
                let guard = shared
                    .sessions
                    .authority_host_sessions
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                guard.contains_key(&session_id)
            };
            if is_host {
                let (cols, rows) = shared
                    .resize
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

        ERROR_LOG.log(format!(
            "[timing] activate_target END target_id={target_id}"
        ));
        CommandOutcome::Ok
    } else {
        CommandOutcome::Error("unknown target".to_string())
    }
}

fn connect_remote_host_target(
    shared: &Arc<SharedState>,
    remote_owner: &RemoteRuntimeOwnerRuntime,
    profile_name: &str,
) -> CommandOutcome {
    let Some(target_registry) = shared.target_registry_port.clone() else {
        return CommandOutcome::Error("target registry port not configured".to_string());
    };
    let Some(session_creation) = shared.session_creation_port.clone() else {
        return CommandOutcome::Error("session creation port not configured".to_string());
    };

    let history_store = RemoteHostHistoryStore::new(RemoteHostHistoryStore::default_path());

    let request = RemoteHostConnectRequest {
        profile_name: Some(profile_name.to_string()),
        direct_profile: None,
        save_profile_name: None,
        replace_profile_name: None,
        local_connect_endpoint: shared.network.advertised_public_endpoint_label(),
        cwd_hint: None,
        use_install_proxy: true,
    };

    let runtime = RemoteHostConnectRuntime::new(
        history_store,
        SshRemotePortProbeFactory,
        SshRemoteHostBootstrapper::default(),
        target_registry,
        session_creation,
    );

    let outcome = match runtime.connect(request) {
        Ok(outcome) => outcome,
        Err(error) => return CommandOutcome::Error(error.to_string()),
    };

    let record = outcome.created_target;
    let target = record.address.qualified_target();

    if let Err(error) = remote_owner.upsert_session(&outcome.authority_node_id, &record) {
        return CommandOutcome::Error(format!("failed to register remote session: {error}"));
    }

    {
        let mut guard = shared
            .sessions
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        guard.retain(|_, session| session.address.id() != record.address.id());
        guard.insert(target.clone(), record.clone());
    }

    if let Err(error) = shared.ensure_remote_session(&record) {
        return CommandOutcome::Error(error.to_string());
    }

    *shared
        .sessions
        .active_target
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some(target.clone());
    let (cols, rows) = shared
        .resize
        .last_client_resize
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .unwrap_or((80, 24));
    shared.resize_active_remote_session(cols, rows);

    CommandOutcome::Message(format!("connected {target}"))
}

fn create_remote_session_on_authority(
    shared: &Arc<SharedState>,
    authority_node_id: &str,
) -> CommandOutcome {
    let Some(session_creation) = shared.session_creation_port.clone() else {
        return CommandOutcome::Error("session creation port not configured".to_string());
    };

    let request = RemoteSessionCreationRequest {
        authority_node_id: authority_node_id.to_string(),
        cwd_hint: None,
        cols: 0,
        rows: 0,
    };

    match session_creation.create_session(request) {
        Ok(record) => {
            let target = record.address.qualified_target();
            {
                let mut guard = shared
                    .sessions
                    .sessions
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                guard.retain(|_, session| session.address.id() != record.address.id());
                guard.insert(target.clone(), record.clone());
            }
            if let Err(error) = shared.ensure_remote_session(&record) {
                return CommandOutcome::Error(format!(
                    "created remote session but failed to open viewer: {error}"
                ));
            }
            *shared
                .sessions
                .active_target
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(target.clone());
            let (cols, rows) = shared
                .resize
                .last_client_resize
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .unwrap_or((80, 24));
            shared.resize_active_remote_session(cols, rows);
            CommandOutcome::Message(format!("created remote session {target}"))
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

#[cfg(test)]
mod state_loop_tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::os::unix::net::UnixStream;
    use std::path::PathBuf;
    use std::sync::{mpsc, Mutex};
    use std::time::Duration;

    /// Serialise the state-loop tests so the shared instrumentation atomics are
    /// not mutated concurrently by multiple loops.
    static STATE_LOOP_TEST_LOCK: Mutex<()> = Mutex::new(());

    use crate::cli::RemoteNetworkConfig;
    use crate::domain::session_catalog::{
        ManagedSessionAddress, ManagedSessionRecord, ManagedSessionTaskState, SessionAvailability,
    };
    use crate::remote::node::remote_node_session_sync_runtime::LocalCatalogChangeRequest;
    use crate::remote::node::remote_runtime_owner_runtime::RemoteRuntimeOwnerRuntime;

    fn start_test_loop() -> (
        Arc<SharedState>,
        mpsc::Sender<StateEvent>,
        super::super::client_writer::ClientWriterHandle,
        std::thread::JoinHandle<()>,
    ) {
        let network = RemoteNetworkConfig::default();
        let shared = SharedState::new(network.clone()).expect("SharedState::new should succeed");
        let (tx, rx) = mpsc::channel::<StateEvent>();
        let (catalog_tx, _catalog_rx) = mpsc::channel::<LocalCatalogChangeRequest>();
        // Do not install the state sender into SharedState; tests close the loop
        // by dropping `tx`, and a lingering clone would prevent the loop from
        // exiting.
        let shared_for_loop = shared.clone();
        let client_writer = super::super::client_writer::ClientWriter::start();
        let client_writer_for_loop = client_writer.clone();
        let remote_owner = RemoteRuntimeOwnerRuntime::new(PathBuf::from("/dev/null"), network);
        let handle = std::thread::spawn(move || {
            let _ = run_state_event_loop(
                shared_for_loop,
                rx,
                catalog_tx,
                super::super::authority_host_io_loop::AuthorityHostIoHandle::dangling(),
                client_writer_for_loop,
                remote_owner,
                SettingsStore::new(std::env::temp_dir().join("waitagent-test-settings.toml")),
            );
        });
        (shared, tx, client_writer, handle)
    }

    #[test]
    fn state_loop_broadcasts_after_local_session_output() {
        let _guard = STATE_LOOP_TEST_LOCK.lock().unwrap();
        let (_shared, tx, client_writer, handle) = start_test_loop();
        let (server, client) = UnixStream::pair().expect("stream pair");
        server
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("set timeout");

        // Register the client stream first, then notify the state loop. This
        // keeps the ClientWriter queue order deterministic: Register, then the
        // ClientConnected broadcast, then the LocalSessionOutput broadcast, then
        // the Status response.
        client_writer.send(super::super::client_writer::ClientWriterRequest::Register {
            client_id: 1,
            stream: client,
        });
        let _ = tx.send(StateEvent::ClientConnected { client_id: 1 });

        // Trigger a snapshot broadcast via the single-writer loop.
        let _ = tx.send(StateEvent::LocalSessionOutput {
            target_id: "local#0:1".to_string(),
        });

        // Send a one-shot status command so we have a deterministic final line.
        let _ = tx.send(StateEvent::ClientCommand {
            client_id: 1,
            command: ClientCommand::Status,
        });

        let mut reader = BufReader::new(server);
        let mut snapshot_line = String::new();
        reader
            .read_line(&mut snapshot_line)
            .expect("first snapshot line should be broadcast");
        assert!(
            snapshot_line.contains("\"type\":\"Snapshot\"") || snapshot_line.contains("Snapshot"),
            "expected first snapshot broadcast, got: {snapshot_line}"
        );

        let mut output_snapshot_line = String::new();
        reader
            .read_line(&mut output_snapshot_line)
            .expect("LocalSessionOutput snapshot line should be broadcast");
        assert!(
            output_snapshot_line.contains("\"type\":\"Snapshot\"")
                || output_snapshot_line.contains("Snapshot"),
            "expected second snapshot broadcast after LocalSessionOutput, got: {output_snapshot_line}"
        );

        let mut status_line = String::new();
        reader
            .read_line(&mut status_line)
            .expect("status response should be written");
        assert!(
            status_line.contains("\"ok\":true"),
            "expected ok status response, got: {status_line}"
        );

        drop(tx);
        drop(client_writer);
        handle.join().expect("state loop should exit cleanly");
    }

    #[test]
    fn state_loop_single_writer_invariant() {
        let _guard = STATE_LOOP_TEST_LOCK.lock().unwrap();
        let (shared, tx, _client_writer, handle) = start_test_loop();

        // Send a catalog update event instead of mutating SharedState directly.
        let record = ManagedSessionRecord {
            address: ManagedSessionAddress::remote_peer(
                "peer#99999".to_string(),
                "sess-1".to_string(),
            ),
            selector: None,
            availability: SessionAvailability::Online,
            workspace_dir: None,
            workspace_key: None,
            session_role: None,
            opened_by: Vec::new(),
            attached_clients: 0,
            window_count: 1,
            command_name: Some("bash".to_string()),
            display_command_name: None,
            current_path: None,
            task_state: ManagedSessionTaskState::Input,
        };
        let target_id = record.address.qualified_target();
        let _ = tx.send(StateEvent::RemoteSessionCatalogUpdated { record });

        // Give the loop a moment to apply the event.
        std::thread::sleep(Duration::from_millis(50));

        // Verify the mutation was applied by the single writer.
        let guard = shared
            .sessions
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        assert!(
            guard.contains_key(&target_id),
            "SharedState should contain the remote session inserted by the state loop"
        );

        drop(tx);
        handle.join().expect("state loop should exit cleanly");
    }

    #[test]
    fn broadcast_snapshot_only_from_state_loop() {
        let _guard = STATE_LOOP_TEST_LOCK.lock().unwrap();
        TEST_BROADCAST_COUNT.store(0, Ordering::SeqCst);
        *TEST_BROADCAST_THREAD_ID
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;

        let (_shared, tx, client_writer, handle) = start_test_loop();
        let loop_thread_id = handle.thread().id();

        let (_server, client) = UnixStream::pair().expect("stream pair");
        client_writer.send(super::super::client_writer::ClientWriterRequest::Register {
            client_id: 2,
            stream: client,
        });
        let _ = tx.send(StateEvent::ClientConnected { client_id: 2 });
        let _ = tx.send(StateEvent::LocalSessionOutput {
            target_id: "local#0:2".to_string(),
        });

        // Wait for the broadcast to be processed.
        std::thread::sleep(Duration::from_millis(50));

        assert!(
            TEST_BROADCAST_COUNT.load(Ordering::SeqCst) > 0,
            "broadcast_snapshot should have been invoked"
        );
        let observed_id = *TEST_BROADCAST_THREAD_ID
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            observed_id,
            Some(loop_thread_id),
            "broadcast_snapshot must run on the StateEventLoop thread"
        );

        drop(tx);
        drop(client_writer);
        handle.join().expect("state loop should exit cleanly");
    }

    #[test]
    fn stress_concurrent_session_and_client_changes() {
        let _guard = STATE_LOOP_TEST_LOCK.lock().unwrap();
        let (shared, tx, client_writer, handle) = start_test_loop();
        let (_server, client) = UnixStream::pair().expect("stream pair");
        client_writer.send(super::super::client_writer::ClientWriterRequest::Register {
            client_id: 42,
            stream: client,
        });

        let tx1 = tx.clone();
        let tx2 = tx.clone();
        let shared1 = shared.clone();

        let t1 = std::thread::spawn(move || {
            for i in 0..50 {
                let _ = tx1.send(StateEvent::LocalSessionOutput {
                    target_id: format!("local#0:{i}"),
                });
            }
        });

        let t2 = std::thread::spawn(move || {
            for i in 0..50 {
                let _ = tx2.send(StateEvent::ClientConnected {
                    client_id: 100 + i as u64,
                });
                let _ = tx2.send(StateEvent::ClientDisconnected {
                    client_id: 100 + i as u64,
                });
            }
        });

        let t3 = std::thread::spawn(move || {
            // Reader threads may concurrently inspect SharedState through
            // history/status APIs while the single writer is processing events.
            for _ in 0..50 {
                let _ = shared1.history_for_target("local#0:0");
                let _ = shared1.clients.client_count.load(Ordering::SeqCst);
            }
        });

        t1.join().unwrap();
        t2.join().unwrap();
        t3.join().unwrap();

        // Give the single writer time to drain the event queue.
        std::thread::sleep(Duration::from_millis(100));
        drop(tx);
        drop(client_writer);
        handle.join().expect("state loop should exit cleanly");
    }
}
