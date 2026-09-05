use crate::domain::agent_detector::{accepts_at_reference, DetectorRegistry};
use crate::domain::agent_signal::AgentStateEffect;
use crate::domain::session_catalog::{
    ManagedSessionRecord, ManagedSessionTaskState, SessionAvailability, SessionTransport,
};
use crate::infra::error_log::ERROR_LOG;
use crate::infra::settings_store::SettingsStore;
use crate::lifecycle::LifecycleError;
use crate::ports::session_creation::RemoteSessionCreationRequest;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::authority_host_io_loop::{
    AuthorityHostIoHandle, AuthorityHostIoLoop, AuthorityHostIoRequest,
};
use super::client_writer::{ClientWriterHandle, ClientWriterRequest};
use super::clipboard_platform::format_file_reference;
use super::inbound_connect_wait_worker::InboundConnectWaitWorker;
use super::key_translation::{translate_key, KeyTranslationMode};
use super::logical_key::LogicalKey;
use super::outbound_dial_retry_worker::OutboundDialRetryWorker;
use super::peer_reachability_probe_worker::PeerReachabilityProbeWorker;
use super::reconnect_worker::ReconnectWorker;
use super::runtime::{RemoteNodeConnectionInfo, RemoteNodeConnectionMode, SharedState};
use super::snapshot::{
    build_snapshot, history_response_json, response_json, snapshot_json, ControlResponse,
    HistoryResponse, ServerStatus, SessionView,
};
use super::state_event::{
    ClientCommand, CommandOutcome, CreatedAuthorityHostTarget, RemoteHostConnectedOutcome,
    StateEvent,
};
use crate::host::ssh::outbound_connection_snapshot_store::OutboundConnectionSnapshotStore;
use crate::host::ssh::remote_host_connect_runtime::{
    RemoteHostConnectRequest, RemoteHostConnectRuntime, SshRemotePortProbeFactory,
};
use crate::host::ssh::remote_host_history_store::RemoteHostHistoryStore;
use crate::host::ssh::ssh_remote_host_bootstrapper::SshRemoteHostBootstrapper;
use crate::infra::remote_grpc_transport::OutboundNodeSessionRequest;
use crate::remote::node::remote_node_ingress_server_runtime::InternalEvent;
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
        let loop_tx = tx.clone();
        let authority_host_io_tx = authority_host_io.sender();
        let snapshot_store =
            OutboundConnectionSnapshotStore::new(OutboundConnectionSnapshotStore::default_path());
        std::thread::spawn(move || {
            if let Err(error) = run_state_event_loop(
                shared,
                loop_tx,
                rx,
                catalog_tx,
                authority_host_io_tx,
                client_writer,
                remote_owner,
                settings_store,
                snapshot_store,
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

#[allow(clippy::too_many_arguments)]
fn run_state_event_loop(
    shared: Arc<SharedState>,
    state_event_tx: mpsc::Sender<StateEvent>,
    rx: mpsc::Receiver<StateEvent>,
    catalog_tx: mpsc::Sender<LocalCatalogChangeRequest>,
    authority_host_io_tx: AuthorityHostIoHandle,
    client_writer: ClientWriterHandle,
    remote_owner: RemoteRuntimeOwnerRuntime,
    settings_store: SettingsStore,
    snapshot_store: OutboundConnectionSnapshotStore,
) -> Result<(), LifecycleError> {
    let mut connected_clients: HashSet<u64> = HashSet::new();
    let mut reconnect_handles: HashMap<String, mpsc::Sender<()>> = HashMap::new();
    let mut outbound_dial_retry_handles: HashMap<String, OutboundDialRetryWorker> = HashMap::new();
    let mut inbound_connect_wait_handles: HashMap<String, InboundConnectWaitWorker> =
        HashMap::new();
    let mut peer_reachability_handles: HashMap<String, PeerReachabilityProbeWorker> =
        HashMap::new();
    let mut last_retry_reset: HashMap<String, Instant> = HashMap::new();
    // Profiles with a connect() currently running (user-initiated or snapshot
    // reconnect).  Both paths dial, wait, and SSH-bootstrap on their own
    // thread; without this guard a network-online flap can overlap a
    // user-initiated connect for the same profile, and the second dial's
    // CloseNodeIngressSession kills the first dial's freshly opened session.
    let mut connecting_profiles: HashSet<String> = HashSet::new();
    // Nodes already known to be offline. `RemoteNodeOffline` can be re-signaled
    // by the ingress close path or by transport failures; without this dedup
    // the handler and the ingress server ping-pong the same event forever and
    // flood the single state-loop thread. Cleared when the node comes back.
    let mut offline_nodes: HashSet<String> = HashSet::new();
    let mut network_online: bool = true;

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

            StateEvent::SessionCommandNameCleared { target_id } => {
                shared.clear_session_command_name(&target_id);
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
                    &mut connecting_profiles,
                    client_id,
                    command,
                    &settings_store,
                    &snapshot_store,
                    state_event_tx.clone(),
                );
            }

            StateEvent::RemoteHostConnectResult {
                client_id,
                profile_name,
                result,
                activate,
            } => {
                connecting_profiles.remove(&profile_name);
                if let Ok(outcome) = result.as_ref() {
                    offline_nodes.remove(&outcome.authority_node_id);
                }
                handle_remote_host_connect_result(
                    &shared,
                    &client_writer,
                    &connected_clients,
                    &snapshot_store,
                    client_id,
                    profile_name,
                    *result,
                    activate,
                );
            }

            StateEvent::RemoteSessionCreateResult {
                client_id,
                authority_node_id,
                result,
            } => {
                handle_remote_session_create_result(
                    &shared,
                    &client_writer,
                    &connected_clients,
                    client_id,
                    &authority_node_id,
                    *result,
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
                    &mut outbound_dial_retry_handles,
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
                shared.update_remote_session_record(*record);
                broadcast_snapshot(&shared, &client_writer, &connected_clients);
            }

            StateEvent::SessionClosed { target_id } => {
                handle_session_closed(
                    &shared,
                    &client_writer,
                    &connected_clients,
                    &mut reconnect_handles,
                    &mut outbound_dial_retry_handles,
                    &snapshot_store,
                    network_online,
                    target_id,
                );
            }

            StateEvent::RemoteNodeOffline { node_id } => {
                // Dedup: the ingress close path and transport failure paths can
                // re-signal offline for a node the loop already handled. The
                // first event marks the node offline; later repeats are
                // absorbed here so the loop cannot self-sustain.
                if !offline_nodes.insert(node_id.clone()) {
                    continue;
                }
                handle_remote_node_offline(
                    &shared,
                    &client_writer,
                    &connected_clients,
                    &mut reconnect_handles,
                    &mut outbound_dial_retry_handles,
                    &mut inbound_connect_wait_handles,
                    &mut peer_reachability_handles,
                    node_id,
                );
            }

            StateEvent::RemoteNodeOnline { node_id } => {
                offline_nodes.remove(&node_id);
                if let Some(worker) = outbound_dial_retry_handles.remove(&node_id) {
                    let _ = worker.cancel_tx.send(());
                }
                if let Some(worker) = inbound_connect_wait_handles.remove(&node_id) {
                    let _ = worker.cancel_tx.send(());
                }
                if let Some(worker) = peer_reachability_handles.remove(&node_id) {
                    let _ = worker.cancel_tx.send(());
                }
                last_retry_reset.remove(&node_id);
            }

            StateEvent::RemoteNodeReachable { node_id } => {
                // Debounce: ignore reachable bursts that arrive shortly after a
                // reset, otherwise a peer whose gRPC handshake is still pending
                // can cause repeated worker restarts.
                const RESET_DEBOUNCE: Duration = Duration::from_secs(5);
                let should_reset = last_retry_reset
                    .get(&node_id)
                    .map(|instant| instant.elapsed() >= RESET_DEBOUNCE)
                    .unwrap_or(true);
                if should_reset {
                    last_retry_reset.insert(node_id.clone(), Instant::now());
                    reset_outbound_dial_retry_worker(
                        &shared,
                        node_id,
                        &mut outbound_dial_retry_handles,
                        &state_event_tx,
                    );
                }
            }

            StateEvent::RemoteNodeReconnectFailed { node_id } => {
                handle_remote_node_reconnect_failed(
                    &shared,
                    &client_writer,
                    &connected_clients,
                    &mut reconnect_handles,
                    &mut outbound_dial_retry_handles,
                    &mut inbound_connect_wait_handles,
                    &mut peer_reachability_handles,
                    node_id,
                );
            }

            StateEvent::RecordRemoteNodeConnection { node_id, info } => {
                shared.record_remote_node_connection(&node_id, info);
            }

            StateEvent::NetworkConnectivityChanged { online } => {
                let was_online = network_online;
                network_online = online;
                if online && !was_online {
                    // The control plane thinks it is back online.  Reset the
                    // retry workers for all offline outbound-dial peers so they
                    // try immediately instead of waiting out a long backoff.
                    let offline_node_ids: std::collections::HashSet<String> = {
                        let guard = shared
                            .sessions
                            .sessions
                            .lock()
                            .unwrap_or_else(|e| e.into_inner());
                        guard
                            .values()
                            .filter(|record| {
                                *record.address.transport() == SessionTransport::RemotePeer
                                    && record.availability == SessionAvailability::Offline
                            })
                            .map(|record| record.address.authority_id().to_string())
                            .collect()
                    };
                    for node_id in offline_node_ids {
                        if shared
                            .remote_node_connection(&node_id)
                            .is_some_and(|info| info.mode == RemoteNodeConnectionMode::OutboundDial)
                        {
                            reset_outbound_dial_retry_worker(
                                &shared,
                                node_id,
                                &mut outbound_dial_retry_handles,
                                &state_event_tx,
                            );
                        }
                    }
                    let _ = shared
                        .state_sender()
                        .send(StateEvent::ReconnectSnapshotHosts);
                }
            }

            StateEvent::ReconnectSnapshotHosts => {
                reconnect_snapshot_hosts(
                    &shared,
                    &remote_owner,
                    &snapshot_store,
                    &state_event_tx,
                    &outbound_dial_retry_handles,
                    &inbound_connect_wait_handles,
                    &mut connecting_profiles,
                );
            }

            StateEvent::SnapshotHostReconnectResult {
                profile_name,
                authority_node_id,
                result,
            } => {
                connecting_profiles.remove(&profile_name);
                match *result {
                    Ok(outcome) => {
                        offline_nodes.remove(&authority_node_id);
                        apply_remote_host_connect_outcome(
                            &shared,
                            &snapshot_store,
                            &profile_name,
                            false,
                            outcome,
                        );
                    }
                    Err(error) => {
                        ERROR_LOG.log(format!(
                        "[ratatui-state-loop] snapshot reconnect failed for profile `{profile_name}` node={authority_node_id}: {error}"
                    ));
                        if network_online {
                            if let Err(remove_error) = snapshot_store.remove(&authority_node_id) {
                                ERROR_LOG.log(format!(
                                "[ratatui-state-loop] failed to remove snapshot for {authority_node_id}: {remove_error}"
                            ));
                            }
                        }
                    }
                }
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
    connecting_profiles: &mut HashSet<String>,
    client_id: u64,
    command: ClientCommand,
    settings_store: &SettingsStore,
    snapshot_store: &OutboundConnectionSnapshotStore,
    state_event_tx: mpsc::Sender<StateEvent>,
) {
    if let ClientCommand::GetHistory { target_id } = &command {
        let response = build_history_response(shared, target_id);
        let payload = history_response_json(&response);
        client_writer.send(ClientWriterRequest::Write { client_id, payload });
        return;
    }

    if let ClientCommand::ConnectRemoteHost { profile_name } = &command {
        connect_remote_host_target(
            shared,
            remote_owner,
            client_id,
            profile_name,
            connecting_profiles,
            state_event_tx,
        );
        return;
    }

    if let ClientCommand::CreateRemoteSession { authority_node_id } = &command {
        create_remote_session_target(shared, client_id, authority_node_id, state_event_tx);
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
            snapshot_store,
            connecting_profiles,
            state_event_tx.clone(),
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
    outbound_dial_retry_handles: &mut HashMap<String, OutboundDialRetryWorker>,
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
            // Capture the record and node id before we mutate the catalog.
            let record = {
                let guard = shared
                    .sessions
                    .sessions
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                guard.get(&target_id).cloned()
            };
            let node_id = record
                .as_ref()
                .filter(|r| *r.address.transport() == SessionTransport::RemotePeer)
                .map(|r| r.address.authority_id().to_string());

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

            // For outbound-dial peers, start a node-level retry worker so the
            // server re-dials the remote waitagent without re-bootstrapping.
            if let Some(node_id) = node_id {
                if let Some(info) = shared.remote_node_connection(&node_id) {
                    if info.mode == RemoteNodeConnectionMode::OutboundDial
                        && !outbound_dial_retry_handles.contains_key(&node_id)
                    {
                        let request = OutboundNodeSessionRequest {
                            node_id: node_id.clone(),
                            endpoint_uri: format!("tls://{}:{}", info.host, info.port),
                            tls_pin_sha256: Some(info.tls_pin_sha256.clone())
                                .filter(|s| !s.is_empty()),
                        };
                        let ingress_tx = {
                            let guard = shared
                                .ingress_internal_tx
                                .lock()
                                .unwrap_or_else(|e| e.into_inner());
                            guard.clone()
                        };
                        if let Some(ingress_tx) = ingress_tx {
                            let worker = OutboundDialRetryWorker::start(
                                node_id.clone(),
                                request,
                                ingress_tx,
                                shared.state_sender(),
                            );
                            outbound_dial_retry_handles.insert(node_id, worker);
                        }
                    }
                }
            }

            // Start a background worker with the last known terminal size.
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

#[allow(clippy::too_many_arguments)]
fn handle_session_closed(
    shared: &Arc<SharedState>,
    client_writer: &ClientWriterHandle,
    connected_clients: &HashSet<u64>,
    reconnect_handles: &mut HashMap<String, mpsc::Sender<()>>,
    outbound_dial_retry_handles: &mut HashMap<String, OutboundDialRetryWorker>,
    snapshot_store: &OutboundConnectionSnapshotStore,
    network_online: bool,
    target_id: String,
) {
    if let Some(tx) = reconnect_handles.remove(&target_id) {
        let _ = tx.send(());
    }
    let node_id = {
        let guard = shared
            .sessions
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        guard
            .get(&target_id)
            .filter(|r| *r.address.transport() == SessionTransport::RemotePeer)
            .map(|r| r.address.authority_id().to_string())
    };
    shared.handle_session_exit(&target_id);
    if let Some(node_id) = node_id {
        if let Some(worker) = outbound_dial_retry_handles.remove(&node_id) {
            let _ = worker.cancel_tx.send(());
        }
        if network_online {
            if let Err(error) = snapshot_store.remove(&node_id) {
                ERROR_LOG.log(format!(
                    "[ratatui-state-loop] failed to remove snapshot for {node_id}: {error}"
                ));
            }
        }
    }
    broadcast_snapshot(shared, client_writer, connected_clients);
}

#[allow(clippy::too_many_arguments)]
fn handle_remote_node_offline(
    shared: &Arc<SharedState>,
    client_writer: &ClientWriterHandle,
    connected_clients: &HashSet<u64>,
    reconnect_handles: &mut HashMap<String, mpsc::Sender<()>>,
    outbound_dial_retry_handles: &mut HashMap<String, OutboundDialRetryWorker>,
    inbound_connect_wait_handles: &mut HashMap<String, InboundConnectWaitWorker>,
    peer_reachability_handles: &mut HashMap<String, PeerReachabilityProbeWorker>,
    node_id: String,
) {
    ERROR_LOG.log(format!(
        "[ratatui-state-loop] remote node offline node={node_id}"
    ));
    // Cancel any active reconnect workers for this node; they were tied to the
    // old transport and will be restarted by RemoteSessionDisconnected events.
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
    // Keep remote_node_connections and the snapshot so retries can recover the
    // endpoint and credentials. Mark sessions offline in the sidebar.
    {
        let mut guard = shared
            .sessions
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for record in guard.values_mut() {
            if *record.address.transport() == SessionTransport::RemotePeer
                && record.address.authority_id() == node_id
            {
                record.availability = SessionAvailability::Offline;
            }
        }
    }
    // For outbound-dial peers, start a node-level retry worker so the control
    // plane re-dials the remote waitagent without requiring SSH re-bootstrap.
    // Evict any stale ingress session first so the fresh dial is not rejected
    // by the duplicate-dial guard.
    close_ingress_sessions_for_node(shared, &node_id);
    if let std::collections::hash_map::Entry::Vacant(slot) =
        outbound_dial_retry_handles.entry(node_id.clone())
    {
        if let Some(info) = shared.remote_node_connection(&node_id) {
            if info.mode == RemoteNodeConnectionMode::OutboundDial {
                let request = OutboundNodeSessionRequest {
                    node_id: node_id.clone(),
                    endpoint_uri: format!("tls://{}:{}", info.host, info.port),
                    tls_pin_sha256: Some(info.tls_pin_sha256.clone()).filter(|s| !s.is_empty()),
                };
                let ingress_tx = {
                    let guard = shared
                        .ingress_internal_tx
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    guard.clone()
                };
                if let Some(ingress_tx) = ingress_tx {
                    let worker = OutboundDialRetryWorker::start(
                        node_id.clone(),
                        request,
                        ingress_tx,
                        shared.state_sender(),
                    );
                    slot.insert(worker);
                }
            }
        }
    }
    // For outbound-dial peers, also start a per-peer L4 reachability probe so
    // LAN flash drops that the public-DNS network probe misses are detected as
    // soon as the peer's TCP port comes back.
    if let std::collections::hash_map::Entry::Vacant(slot) =
        peer_reachability_handles.entry(node_id.clone())
    {
        if let Some(info) = shared.remote_node_connection(&node_id) {
            if info.mode == RemoteNodeConnectionMode::OutboundDial {
                let worker = PeerReachabilityProbeWorker::start(
                    node_id.clone(),
                    info.host.clone(),
                    info.port,
                    shared.state_sender(),
                );
                slot.insert(worker);
            }
        }
    }
    // For inbound `--connect` peers, start a wait worker. The timeout depends
    // on whether the control host can reach the peer's listening port.
    if let std::collections::hash_map::Entry::Vacant(slot) =
        inbound_connect_wait_handles.entry(node_id.clone())
    {
        if let Some(info) = shared.remote_node_connection(&node_id) {
            if info.mode == RemoteNodeConnectionMode::InboundConnect {
                const INBOUND_LAN_OFFLINE_TIMEOUT: Duration = Duration::from_secs(120);
                const INBOUND_CLOUD_OFFLINE_TIMEOUT: Duration = Duration::from_secs(300);
                let timeout = if info.server_can_reach_peer {
                    INBOUND_LAN_OFFLINE_TIMEOUT
                } else {
                    INBOUND_CLOUD_OFFLINE_TIMEOUT
                };
                let worker = InboundConnectWaitWorker::start(
                    node_id.clone(),
                    timeout,
                    shared.state_sender(),
                );
                slot.insert(worker);
            }
        }
    }
    broadcast_snapshot(shared, client_writer, connected_clients);
}

#[allow(clippy::too_many_arguments)]
fn handle_remote_node_reconnect_failed(
    shared: &Arc<SharedState>,
    client_writer: &ClientWriterHandle,
    connected_clients: &HashSet<u64>,
    reconnect_handles: &mut HashMap<String, mpsc::Sender<()>>,
    outbound_dial_retry_handles: &mut HashMap<String, OutboundDialRetryWorker>,
    inbound_connect_wait_handles: &mut HashMap<String, InboundConnectWaitWorker>,
    _peer_reachability_handles: &mut HashMap<String, PeerReachabilityProbeWorker>,
    node_id: String,
) {
    ERROR_LOG.log(format!(
        "[ratatui-state-loop] remote node reconnect failed node={node_id}"
    ));
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
        shared.handle_session_exit(&target_id);
    }
    // The retry worker that reported failure is already exiting; clean up the
    // handle.  The wait worker (inbound `--connect` peers) has also timed out.
    if let Some(worker) = outbound_dial_retry_handles.remove(&node_id) {
        let _ = worker.cancel_tx.send(());
    }
    if let Some(worker) = inbound_connect_wait_handles.remove(&node_id) {
        let _ = worker.cancel_tx.send(());
    }
    // Keep the reachability probe running so we detect when the peer comes back.
    // Also keep the stored snapshot and remote_node_connection so a later
    // reconnect can reuse the same endpoint and credentials instead of
    // re-bootstrapping via SSH.
    close_ingress_sessions_for_node(shared, &node_id);
    broadcast_snapshot(shared, client_writer, connected_clients);
}

/// Ask the ingress server to drop any active or pending gRPC session for
/// `node_id`.  This is needed when the control plane knows a remote peer is
/// offline but the ingress runtime may still hold a stale session that would
/// block a new dial.
fn close_ingress_sessions_for_node(shared: &Arc<SharedState>, node_id: &str) {
    let ingress_tx = {
        let guard = shared
            .ingress_internal_tx
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        guard.clone()
    };
    if let Some(ingress_tx) = ingress_tx {
        let _ = ingress_tx.send(InternalEvent::CloseNodeIngressSession {
            node_id: node_id.to_string(),
        });
    }
}

/// Cancel any existing outbound-dial retry worker for `node_id` and start a
/// fresh one with the initial backoff.  Call this when we have reason to believe
/// the peer may be reachable again (network recovery or L4 probe success).
fn reset_outbound_dial_retry_worker(
    shared: &Arc<SharedState>,
    node_id: String,
    outbound_dial_retry_handles: &mut HashMap<String, OutboundDialRetryWorker>,
    state_event_tx: &mpsc::Sender<StateEvent>,
) {
    let Some(info) = shared.remote_node_connection(&node_id) else {
        return;
    };
    if info.mode != RemoteNodeConnectionMode::OutboundDial {
        return;
    }

    ERROR_LOG.log(format!(
        "[ratatui-state-loop] resetting outbound-dial retry worker for {node_id}"
    ));

    if let Some(worker) = outbound_dial_retry_handles.remove(&node_id) {
        let _ = worker.cancel_tx.send(());
    }

    // Evict any stale ingress session before starting a fresh dial attempt;
    // otherwise the duplicate-dial guard in the ingress server would reject the
    // new connection while the old, broken session is still in its map.
    close_ingress_sessions_for_node(shared, &node_id);

    let request = OutboundNodeSessionRequest {
        node_id: node_id.clone(),
        endpoint_uri: format!("tls://{}:{}", info.host, info.port),
        tls_pin_sha256: Some(info.tls_pin_sha256.clone()).filter(|s| !s.is_empty()),
    };
    let ingress_tx = {
        let guard = shared
            .ingress_internal_tx
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        guard.clone()
    };
    if let Some(ingress_tx) = ingress_tx {
        let worker = OutboundDialRetryWorker::start(
            node_id.clone(),
            request,
            ingress_tx,
            state_event_tx.clone(),
        );
        outbound_dial_retry_handles.insert(node_id, worker);
    }
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
    _snapshot_store: &OutboundConnectionSnapshotStore,
    connecting_profiles: &mut HashSet<String>,
    state_event_tx: mpsc::Sender<StateEvent>,
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
            crate::platform::local_ipc::wake_listener(shared.network.port);
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
            connect_remote_host_target(
                shared,
                remote_owner,
                0, // client_id is filled in by the event-loop dispatcher above
                &profile_name,
                connecting_profiles,
                state_event_tx,
            )
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
            let same_size = {
                let guard = shared
                    .resize
                    .last_client_resize
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                *guard == Some((cols, rows))
            };
            if same_size {
                return CommandOutcome::Ok;
            }
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

        ClientCommand::CreateRemoteSession { .. } => {
            // CreateRemoteSession is handled asynchronously in the event-loop
            // dispatcher so it does not block the single writer thread.
            CommandOutcome::Error(
                "CreateRemoteSession must be handled by the event loop".to_string(),
            )
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
    #[cfg(unix)]
    let pty_master = session.pty_master.try_clone().map_err(|error| {
        LifecycleError::Io(
            "failed to clone authority host pty master".to_string(),
            error,
        )
    })?;
    #[cfg(windows)]
    let conpty = session.conpty.take().ok_or_else(|| {
        LifecycleError::Protocol("authority host session missing ConPTY".to_string())
    })?;
    let child = session.child.take().ok_or_else(|| {
        LifecycleError::Protocol("authority host session missing child process".to_string())
    })?;
    let _ = authority_host_io_tx.send(AuthorityHostIoRequest::RegisterSession {
        session_id: session_id.clone(),
        #[cfg(unix)]
        pty_master,
        #[cfg(windows)]
        conpty,
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

/// Wrap `bytes` in XTerm bracketed-paste markers so the receiving shell treats
/// the whole block as a single paste instead of executing embedded newlines.
fn wrap_bracketed_paste(bytes: &[u8]) -> Vec<u8> {
    let mut wrapped = Vec::with_capacity(bytes.len().saturating_add(12));
    wrapped.extend_from_slice(b"\x1b[200~");
    wrapped.extend_from_slice(bytes);
    wrapped.extend_from_slice(b"\x1b[201~");
    wrapped
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
                let wrap_bracketed = {
                    let term = session.term.lock();
                    term.mode()
                        .contains(alacritty_terminal::term::TermMode::BRACKETED_PASTE)
                };
                if wrap_bracketed {
                    session.feed_input(wrap_bracketed_paste(&bytes));
                } else {
                    session.feed_input(bytes);
                }
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

    let supports_at = record
        .agent_command_name
        .as_deref()
        .map(accepts_at_reference)
        .unwrap_or(false);

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

            let path_ref = format_file_reference(&cached_path.to_string_lossy(), supports_at);

            let local_session = {
                let guard = shared
                    .sessions
                    .local_sessions
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                guard.get(target_id).cloned()
            };
            if let Some(session) = local_session {
                session.feed_input(path_ref.into_bytes());
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
                    let _ = _authority_host_io_tx.send(AuthorityHostIoRequest::WriteInput {
                        session_id,
                        bytes: path_ref.into_bytes(),
                    });
                    return CommandOutcome::Ok;
                }
            }

            CommandOutcome::Error("local session not found".to_string())
        }
        SessionTransport::RemotePeer => {
            shared.feed_remote_session_paste_file(target_id, filename_hint, &bytes);
            CommandOutcome::Ok
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
        // Skip the resize when we are re-activating the already-active target;
        // the remote PTY already has the correct dimensions.
        if *record.address.transport() == SessionTransport::RemotePeer
            && previous_target.as_deref() != Some(target_id)
        {
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
            if is_host && previous_target.as_deref() != Some(target_id) {
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
    client_id: u64,
    profile_name: &str,
    connecting_profiles: &mut HashSet<String>,
    state_event_tx: mpsc::Sender<StateEvent>,
) -> CommandOutcome {
    if !connecting_profiles.insert(profile_name.to_string()) {
        return CommandOutcome::Error(format!(
            "connect already in progress for profile `{profile_name}`"
        ));
    }
    let profile_name = profile_name.to_string();
    let shared = shared.clone();
    let remote_owner = remote_owner.clone();
    let state_event_tx = state_event_tx.clone();
    std::thread::spawn(move || {
        let result = perform_remote_host_connect(&shared, &remote_owner, &profile_name);
        let _ = state_event_tx.send(StateEvent::RemoteHostConnectResult {
            client_id,
            profile_name: profile_name.clone(),
            result: Box::new(result),
            activate: true,
        });
    });
    CommandOutcome::Ok
}

fn perform_remote_host_connect(
    shared: &Arc<SharedState>,
    remote_owner: &RemoteRuntimeOwnerRuntime,
    profile_name: &str,
) -> Result<RemoteHostConnectedOutcome, String> {
    let Some(target_registry) = shared.target_registry_port.clone() else {
        return Err("target registry port not configured".to_string());
    };
    let Some(session_creation) = shared.session_creation_port.clone() else {
        return Err("session creation port not configured".to_string());
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
        history_store.clone(),
        SshRemotePortProbeFactory,
        SshRemoteHostBootstrapper::default(),
        target_registry,
        session_creation,
    );

    let outcome = runtime
        .connect(request, |request: OutboundNodeSessionRequest| {
            let guard = shared
                .ingress_internal_tx
                .lock()
                .map_err(|_| "remote node ingress lock is poisoned".to_string())?;
            let tx = guard
                .as_ref()
                .ok_or_else(|| "remote node ingress is not ready".to_string())?;
            // A stale ingress session from a previous disconnect may still be
            // registered for this node.  Evict it before queuing the new dial
            // so the duplicate-dial guard does not reject the reuse attempt.
            let node_id = request.node_id.clone();
            let _ = tx.send(InternalEvent::CloseNodeIngressSession { node_id });
            tx.send(InternalEvent::InitiateOutboundConnection { request })
                .map_err(|_| "remote node ingress is not ready".to_string())
        })
        .map_err(|error| error.to_string())?;

    let record = outcome.created_target;
    let target = record.address.qualified_target();

    // Reload the profile so we capture the TLS pin / port that connect() saved.
    let profile = history_store.load().ok().and_then(|store| {
        store
            .hosts
            .into_iter()
            .find(|profile| profile.name == profile_name)
    });

    let connection_info = profile.map(|profile| RemoteNodeConnectionInfo {
        mode: RemoteNodeConnectionMode::OutboundDial,
        host: profile.host.clone(),
        port: outcome
            .authority_node_id
            .rsplit_once('#')
            .and_then(|(_, port)| port.parse().ok())
            .unwrap_or(0),
        tls_pin_sha256: profile.tls_pin_sha256.clone().unwrap_or_default(),
        profile_name: profile.name.clone(),
        server_can_reach_peer: true,
    });

    remote_owner
        .upsert_session(&outcome.authority_node_id, &record)
        .map_err(|error| format!("failed to register remote session: {error}"))?;

    Ok(RemoteHostConnectedOutcome {
        target_id: target,
        authority_node_id: outcome.authority_node_id,
        created_target: record,
        connection_info,
    })
}

fn apply_remote_host_connect_outcome(
    shared: &Arc<SharedState>,
    snapshot_store: &OutboundConnectionSnapshotStore,
    profile_name: &str,
    activate: bool,
    outcome: RemoteHostConnectedOutcome,
) -> CommandOutcome {
    if let Some(info) = outcome.connection_info {
        shared.record_remote_node_connection(&outcome.authority_node_id, info);
    }

    let record = outcome.created_target;
    let target = outcome.target_id;

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

    if activate {
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
    }

    if let Err(error) = snapshot_store.upsert(profile_name, &outcome.authority_node_id) {
        ERROR_LOG.log(format!(
            "[ratatui-state-loop] connected {target} but failed to write snapshot: {error}"
        ));
    }

    CommandOutcome::Message(format!("connected {target}"))
}

#[allow(clippy::too_many_arguments)]
fn handle_remote_host_connect_result(
    shared: &Arc<SharedState>,
    client_writer: &ClientWriterHandle,
    _connected_clients: &HashSet<u64>,
    snapshot_store: &OutboundConnectionSnapshotStore,
    client_id: u64,
    profile_name: String,
    result: Result<RemoteHostConnectedOutcome, String>,
    activate: bool,
) {
    let outcome = match result {
        Ok(outcome) => apply_remote_host_connect_outcome(
            shared,
            snapshot_store,
            &profile_name,
            activate,
            outcome,
        ),
        Err(message) => CommandOutcome::Error(message),
    };
    let response: ControlResponse = outcome.into();
    let payload = response_json(&response);
    client_writer.send(ClientWriterRequest::Write { client_id, payload });
}

fn reconnect_snapshot_hosts(
    shared: &Arc<SharedState>,
    remote_owner: &RemoteRuntimeOwnerRuntime,
    snapshot_store: &OutboundConnectionSnapshotStore,
    state_event_tx: &mpsc::Sender<StateEvent>,
    outbound_dial_retry_handles: &HashMap<String, OutboundDialRetryWorker>,
    inbound_connect_wait_handles: &HashMap<String, InboundConnectWaitWorker>,
    connecting_profiles: &mut HashSet<String>,
) {
    let entries = match snapshot_store.load() {
        Ok(entries) => entries,
        Err(error) => {
            ERROR_LOG.log(format!(
                "[ratatui-state-loop] failed to load outbound connection snapshot: {error}"
            ));
            return;
        }
    };

    for entry in entries {
        let already_connected = {
            let guard = shared
                .remote_node_connections
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            guard.contains_key(&entry.authority_node_id)
        };
        if already_connected {
            continue;
        }

        let has_online_session = {
            let guard = shared
                .sessions
                .sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            guard.values().any(|record| {
                *record.address.transport() == SessionTransport::RemotePeer
                    && record.address.authority_id() == entry.authority_node_id
                    && record.availability == SessionAvailability::Online
            })
        };
        if has_online_session {
            continue;
        }

        // If a retry or wait worker is already active for this node, let it
        // handle the reconnect.  Spawning a parallel reuse-dial/SSH bootstrap
        // here would race with the worker's backoff and likely fall back to SSH
        // after only 5s.
        if outbound_dial_retry_handles.contains_key(&entry.authority_node_id)
            || inbound_connect_wait_handles.contains_key(&entry.authority_node_id)
        {
            continue;
        }

        // Skip profiles with a connect already in flight (e.g. a
        // user-initiated Ctrl+W connect still waiting for its dial).  A
        // parallel connect for the same profile would evict the in-flight
        // dial's fresh session via CloseNodeIngressSession.
        if !connecting_profiles.insert(entry.profile_name.clone()) {
            continue;
        }

        let shared = Arc::clone(shared);
        let remote_owner = remote_owner.clone();
        let profile_name = entry.profile_name.clone();
        let tx = state_event_tx.clone();
        let authority_node_id = entry.authority_node_id.clone();
        std::thread::spawn(move || {
            let result = perform_remote_host_connect(&shared, &remote_owner, &profile_name);
            let _ = tx.send(StateEvent::SnapshotHostReconnectResult {
                profile_name,
                authority_node_id,
                result: Box::new(result),
            });
        });
    }
}

fn create_remote_session_target(
    shared: &Arc<SharedState>,
    client_id: u64,
    authority_node_id: &str,
    state_event_tx: mpsc::Sender<StateEvent>,
) {
    let authority_node_id = authority_node_id.to_string();
    let shared = shared.clone();
    std::thread::spawn(move || {
        let result = perform_create_remote_session_on_authority(&shared, &authority_node_id);
        let _ = state_event_tx.send(StateEvent::RemoteSessionCreateResult {
            client_id,
            authority_node_id: authority_node_id.clone(),
            result: Box::new(result),
        });
    });
}

fn perform_create_remote_session_on_authority(
    shared: &Arc<SharedState>,
    authority_node_id: &str,
) -> Result<ManagedSessionRecord, String> {
    let Some(session_creation) = shared.session_creation_port.clone() else {
        return Err("session creation port not configured".to_string());
    };

    let request = RemoteSessionCreationRequest {
        authority_node_id: authority_node_id.to_string(),
        cwd_hint: None,
        cols: 0,
        rows: 0,
    };

    session_creation
        .create_session(request)
        .map_err(|error| error.to_string())
}

fn handle_remote_session_create_result(
    shared: &Arc<SharedState>,
    client_writer: &ClientWriterHandle,
    connected_clients: &HashSet<u64>,
    client_id: u64,
    _authority_node_id: &str,
    result: Result<ManagedSessionRecord, String>,
) {
    let outcome = match result {
        Ok(record) => apply_created_remote_session(shared, &record),
        Err(message) => CommandOutcome::Error(message),
    };
    if matches!(outcome, CommandOutcome::Message(_)) {
        broadcast_snapshot(shared, client_writer, connected_clients);
    }
    let response: ControlResponse = outcome.into();
    let payload = response_json(&response);
    client_writer.send(ClientWriterRequest::Write { client_id, payload });
}

fn apply_created_remote_session(
    shared: &Arc<SharedState>,
    record: &ManagedSessionRecord,
) -> CommandOutcome {
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
    if let Err(error) = shared.ensure_remote_session(record) {
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

#[cfg(all(test, unix))]
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
    use crate::ratatui_node::runtime::{RemoteNodeConnectionInfo, RemoteNodeConnectionMode};
    use crate::remote::node::remote_node_session_sync_runtime::LocalCatalogChangeRequest;
    use crate::remote::node::remote_runtime_owner_runtime::RemoteRuntimeOwnerRuntime;

    fn start_test_loop() -> (
        Arc<SharedState>,
        mpsc::Sender<StateEvent>,
        super::super::client_writer::ClientWriterHandle,
        std::thread::JoinHandle<()>,
    ) {
        let snapshot_store = OutboundConnectionSnapshotStore::new(std::env::temp_dir().join(
            format!("waitagent-test-outbound-snapshot-{}", std::process::id()),
        ));
        start_test_loop_with_snapshot_store(snapshot_store)
    }

    fn start_test_loop_with_snapshot_store(
        snapshot_store: OutboundConnectionSnapshotStore,
    ) -> (
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
        // exiting. Use a dangling sender for the loop's self-event channel.
        let (loop_tx, _loop_rx) = mpsc::channel::<StateEvent>();
        let shared_for_loop = shared.clone();
        let client_writer = super::super::client_writer::ClientWriter::start();
        let client_writer_for_loop = client_writer.clone();
        let remote_owner = RemoteRuntimeOwnerRuntime::new(PathBuf::from("/dev/null"), network);
        let handle = std::thread::spawn(move || {
            let _ = run_state_event_loop(
                shared_for_loop,
                loop_tx,
                rx,
                catalog_tx,
                super::super::authority_host_io_loop::AuthorityHostIoHandle::dangling(),
                client_writer_for_loop,
                remote_owner,
                SettingsStore::new(std::env::temp_dir().join("waitagent-test-settings.toml")),
                snapshot_store,
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
            stream: crate::platform::local_ipc::unix::LocalStream::from_unix(client),
            broadcast: true,
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
            agent_command_name: None,
            current_path: None,
            task_state: ManagedSessionTaskState::Input,
        };
        let target_id = record.address.qualified_target();
        let _ = tx.send(StateEvent::RemoteSessionCatalogUpdated {
            record: Box::new(record),
        });

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
            stream: crate::platform::local_ipc::unix::LocalStream::from_unix(client),
            broadcast: true,
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
            stream: crate::platform::local_ipc::unix::LocalStream::from_unix(client),
            broadcast: true,
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

    #[test]
    fn state_loop_clears_display_command_name() {
        let _guard = STATE_LOOP_TEST_LOCK.lock().unwrap();
        let (shared, tx, _client_writer, handle) = start_test_loop();

        let target_id = "local#0:1".to_string();
        {
            let record = ManagedSessionRecord {
                address: ManagedSessionAddress::local("local#0", "1"),
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
                agent_command_name: None,
                current_path: None,
                task_state: ManagedSessionTaskState::Input,
            };
            let mut guard = shared
                .sessions
                .sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            guard.insert(target_id.clone(), record);
        }

        let _ = tx.send(StateEvent::SessionCommandNameChanged {
            target_id: target_id.clone(),
            command_name: "chrome".to_string(),
        });
        let _ = tx.send(StateEvent::SessionCommandNameCleared {
            target_id: target_id.clone(),
        });

        std::thread::sleep(Duration::from_millis(50));

        let guard = shared
            .sessions
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let record = guard.get(&target_id).expect("session should exist");
        assert_eq!(
            record.display_command_name.as_deref(),
            None,
            "display_command_name should be cleared"
        );

        drop(tx);
        handle.join().expect("state loop should exit cleanly");
    }

    #[test]
    fn snapshot_removed_on_session_closed_when_online() {
        let _guard = STATE_LOOP_TEST_LOCK.lock().unwrap();
        let snapshot_path = std::env::temp_dir().join(format!(
            "waitagent-snapshot-close-online-{}",
            std::process::id()
        ));
        let snapshot_store = OutboundConnectionSnapshotStore::new(&snapshot_path);
        snapshot_store
            .upsert("test-profile", "peer#99999")
            .expect("upsert should succeed");

        let (shared, tx, _client_writer, handle) =
            start_test_loop_with_snapshot_store(snapshot_store);

        let target_id = "peer#99999:sess-1".to_string();
        {
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
                agent_command_name: None,
                current_path: None,
                task_state: ManagedSessionTaskState::Input,
            };
            let mut guard = shared
                .sessions
                .sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            guard.insert(target_id.clone(), record);
        }

        let _ = tx.send(StateEvent::SessionClosed {
            target_id: target_id.clone(),
        });

        std::thread::sleep(Duration::from_millis(50));

        let reloaded = OutboundConnectionSnapshotStore::new(&snapshot_path);
        assert_eq!(
            reloaded.load().expect("load should succeed").len(),
            0,
            "snapshot should be removed while network is online"
        );

        drop(tx);
        handle.join().expect("state loop should exit cleanly");
        crate::infra::best_effort::remove_file(&snapshot_path);
    }

    #[test]
    fn snapshot_kept_on_session_closed_when_offline() {
        let _guard = STATE_LOOP_TEST_LOCK.lock().unwrap();
        let snapshot_path = std::env::temp_dir().join(format!(
            "waitagent-snapshot-close-offline-{}",
            std::process::id()
        ));
        let snapshot_store = OutboundConnectionSnapshotStore::new(&snapshot_path);
        snapshot_store
            .upsert("test-profile", "peer#99999")
            .expect("upsert should succeed");

        let (shared, tx, _client_writer, handle) =
            start_test_loop_with_snapshot_store(snapshot_store);

        // Mark network offline before the session closes.
        let _ = tx.send(StateEvent::NetworkConnectivityChanged { online: false });

        let target_id = "peer#99999:sess-1".to_string();
        {
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
                agent_command_name: None,
                current_path: None,
                task_state: ManagedSessionTaskState::Input,
            };
            let mut guard = shared
                .sessions
                .sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            guard.insert(target_id.clone(), record);
        }

        let _ = tx.send(StateEvent::SessionClosed {
            target_id: target_id.clone(),
        });

        std::thread::sleep(Duration::from_millis(50));

        let reloaded = OutboundConnectionSnapshotStore::new(&snapshot_path);
        let entries = reloaded.load().expect("load should succeed");
        assert_eq!(
            entries.len(),
            1,
            "snapshot should be kept while network is offline"
        );
        assert_eq!(entries[0].authority_node_id, "peer#99999");

        drop(tx);
        handle.join().expect("state loop should exit cleanly");
        crate::infra::best_effort::remove_file(&snapshot_path);
    }

    #[test]
    fn snapshot_kept_on_remote_node_offline_when_online() {
        let _guard = STATE_LOOP_TEST_LOCK.lock().unwrap();
        let snapshot_path = std::env::temp_dir().join(format!(
            "waitagent-snapshot-node-online-{}",
            std::process::id()
        ));
        let snapshot_store = OutboundConnectionSnapshotStore::new(&snapshot_path);
        snapshot_store
            .upsert("test-profile", "peer#99999")
            .expect("upsert should succeed");

        let (_shared, tx, _client_writer, handle) =
            start_test_loop_with_snapshot_store(snapshot_store);

        let _ = tx.send(StateEvent::RemoteNodeOffline {
            node_id: "peer#99999".to_string(),
        });

        std::thread::sleep(Duration::from_millis(50));

        let reloaded = OutboundConnectionSnapshotStore::new(&snapshot_path);
        assert_eq!(
            reloaded.load().expect("load should succeed").len(),
            1,
            "snapshot should be kept while retrying after remote node goes offline"
        );

        drop(tx);
        handle.join().expect("state loop should exit cleanly");
        crate::infra::best_effort::remove_file(&snapshot_path);
    }

    #[test]
    fn snapshot_kept_on_remote_node_offline_when_offline() {
        let _guard = STATE_LOOP_TEST_LOCK.lock().unwrap();
        let snapshot_path = std::env::temp_dir().join(format!(
            "waitagent-snapshot-node-offline-{}",
            std::process::id()
        ));
        let snapshot_store = OutboundConnectionSnapshotStore::new(&snapshot_path);
        snapshot_store
            .upsert("test-profile", "peer#99999")
            .expect("upsert should succeed");

        let (_shared, tx, _client_writer, handle) =
            start_test_loop_with_snapshot_store(snapshot_store);

        let _ = tx.send(StateEvent::NetworkConnectivityChanged { online: false });
        let _ = tx.send(StateEvent::RemoteNodeOffline {
            node_id: "peer#99999".to_string(),
        });

        std::thread::sleep(Duration::from_millis(50));

        let reloaded = OutboundConnectionSnapshotStore::new(&snapshot_path);
        let entries = reloaded.load().expect("load should succeed");
        assert_eq!(
            entries.len(),
            1,
            "snapshot should be kept when remote node goes offline while control plane is offline"
        );
        assert_eq!(entries[0].authority_node_id, "peer#99999");

        drop(tx);
        handle.join().expect("state loop should exit cleanly");
        crate::infra::best_effort::remove_file(&snapshot_path);
    }

    #[test]
    fn duplicate_remote_node_offline_is_deduped() {
        let _guard = STATE_LOOP_TEST_LOCK.lock().unwrap();
        let (_shared, tx, client_writer, handle) = start_test_loop();

        // Attach a client so broadcast_snapshot actually broadcasts.
        let (server, client) = UnixStream::pair().expect("stream pair");
        client_writer.send(super::super::client_writer::ClientWriterRequest::Register {
            client_id: 1,
            stream: crate::platform::local_ipc::unix::LocalStream::from_unix(client),
            broadcast: true,
        });
        let _ = tx.send(StateEvent::ClientConnected { client_id: 1 });
        // Let the ClientConnected broadcast settle before counting.
        std::thread::sleep(Duration::from_millis(100));

        TEST_BROADCAST_COUNT.store(0, Ordering::SeqCst);
        for _ in 0..3 {
            let _ = tx.send(StateEvent::RemoteNodeOffline {
                node_id: "peer#99998".to_string(),
            });
        }
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(
            TEST_BROADCAST_COUNT.load(Ordering::SeqCst),
            1,
            "duplicate RemoteNodeOffline events for an already-offline node must be deduped"
        );

        // RemoteNodeOnline clears the dedup state so a genuine later offline
        // is processed again.
        let _ = tx.send(StateEvent::RemoteNodeOnline {
            node_id: "peer#99998".to_string(),
        });
        let _ = tx.send(StateEvent::RemoteNodeOffline {
            node_id: "peer#99998".to_string(),
        });
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(
            TEST_BROADCAST_COUNT.load(Ordering::SeqCst),
            2,
            "offline after online must be processed again"
        );

        drop(server);
        drop(tx);
        handle.join().expect("state loop should exit cleanly");
    }

    #[test]
    fn snapshot_kept_on_remote_node_reconnect_failed_when_online() {
        let _guard = STATE_LOOP_TEST_LOCK.lock().unwrap();
        let snapshot_path = std::env::temp_dir().join(format!(
            "waitagent-snapshot-reconnect-failed-online-{}",
            std::process::id()
        ));
        let snapshot_store = OutboundConnectionSnapshotStore::new(&snapshot_path);
        snapshot_store
            .upsert("test-profile", "peer#99999")
            .expect("upsert should succeed");

        let (_shared, tx, _client_writer, handle) =
            start_test_loop_with_snapshot_store(snapshot_store);

        let _ = tx.send(StateEvent::RemoteNodeReconnectFailed {
            node_id: "peer#99999".to_string(),
        });

        std::thread::sleep(Duration::from_millis(50));

        let reloaded = OutboundConnectionSnapshotStore::new(&snapshot_path);
        let entries = reloaded.load().expect("load should succeed");
        assert_eq!(
            entries.len(),
            1,
            "snapshot should be kept when remote node reconnect fails while network is online"
        );
        assert_eq!(entries[0].authority_node_id, "peer#99999");

        drop(tx);
        handle.join().expect("state loop should exit cleanly");
        crate::infra::best_effort::remove_file(&snapshot_path);
    }

    #[test]
    fn snapshot_kept_on_remote_node_reconnect_failed_when_offline() {
        let _guard = STATE_LOOP_TEST_LOCK.lock().unwrap();
        let snapshot_path = std::env::temp_dir().join(format!(
            "waitagent-snapshot-reconnect-failed-offline-{}",
            std::process::id()
        ));
        let snapshot_store = OutboundConnectionSnapshotStore::new(&snapshot_path);
        snapshot_store
            .upsert("test-profile", "peer#99999")
            .expect("upsert should succeed");

        let (_shared, tx, _client_writer, handle) =
            start_test_loop_with_snapshot_store(snapshot_store);

        let _ = tx.send(StateEvent::NetworkConnectivityChanged { online: false });
        let _ = tx.send(StateEvent::RemoteNodeReconnectFailed {
            node_id: "peer#99999".to_string(),
        });

        std::thread::sleep(Duration::from_millis(50));

        let reloaded = OutboundConnectionSnapshotStore::new(&snapshot_path);
        let entries = reloaded.load().expect("load should succeed");
        assert_eq!(
            entries.len(),
            1,
            "snapshot should be kept when reconnect fails while control plane is offline"
        );
        assert_eq!(entries[0].authority_node_id, "peer#99999");

        drop(tx);
        handle.join().expect("state loop should exit cleanly");
        crate::infra::best_effort::remove_file(&snapshot_path);
    }

    #[test]
    fn network_recovery_triggers_snapshot_reconnect_event() {
        let _guard = STATE_LOOP_TEST_LOCK.lock().unwrap();
        let snapshot_path =
            std::env::temp_dir().join(format!("waitagent-snapshot-recover-{}", std::process::id()));
        let snapshot_store = OutboundConnectionSnapshotStore::new(&snapshot_path);
        snapshot_store
            .upsert("missing-profile", "peer#99999")
            .expect("upsert should succeed");

        let (_shared, tx, _client_writer, handle) =
            start_test_loop_with_snapshot_store(snapshot_store);

        // Transition from offline to online. In production this enqueues
        // ReconnectSnapshotHosts via the loop's self-event channel; in the test
        // harness that channel is dangling, so no reconnect thread runs and the
        // snapshot entry is left untouched. This test only verifies the event is
        // handled without panicking.
        let _ = tx.send(StateEvent::NetworkConnectivityChanged { online: false });
        let _ = tx.send(StateEvent::NetworkConnectivityChanged { online: true });

        std::thread::sleep(Duration::from_millis(100));

        let reloaded = OutboundConnectionSnapshotStore::new(&snapshot_path);
        let entries = reloaded.load().expect("load should succeed");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].profile_name, "missing-profile");

        drop(tx);
        handle.join().expect("state loop should exit cleanly");
        crate::infra::best_effort::remove_file(&snapshot_path);
    }

    #[test]
    fn snapshot_removed_on_snapshot_reconnect_failed_when_online() {
        let _guard = STATE_LOOP_TEST_LOCK.lock().unwrap();
        let snapshot_path = std::env::temp_dir().join(format!(
            "waitagent-snapshot-startup-reconnect-failed-online-{}",
            std::process::id()
        ));
        let snapshot_store = OutboundConnectionSnapshotStore::new(&snapshot_path);
        snapshot_store
            .upsert("test-profile", "peer#99999")
            .expect("upsert should succeed");

        let (_shared, tx, _client_writer, handle) =
            start_test_loop_with_snapshot_store(snapshot_store);

        let _ = tx.send(StateEvent::SnapshotHostReconnectResult {
            profile_name: "test-profile".to_string(),
            authority_node_id: "peer#99999".to_string(),
            result: Box::new(Err("profile not found".to_string())),
        });

        std::thread::sleep(Duration::from_millis(50));

        let reloaded = OutboundConnectionSnapshotStore::new(&snapshot_path);
        assert_eq!(
            reloaded.load().expect("load should succeed").len(),
            0,
            "snapshot should be removed when snapshot reconnect fails while network is online"
        );

        drop(tx);
        handle.join().expect("state loop should exit cleanly");
        crate::infra::best_effort::remove_file(&snapshot_path);
    }

    #[test]
    fn snapshot_kept_on_snapshot_reconnect_failed_when_offline() {
        let _guard = STATE_LOOP_TEST_LOCK.lock().unwrap();
        let snapshot_path = std::env::temp_dir().join(format!(
            "waitagent-snapshot-startup-reconnect-failed-offline-{}",
            std::process::id()
        ));
        let snapshot_store = OutboundConnectionSnapshotStore::new(&snapshot_path);
        snapshot_store
            .upsert("test-profile", "peer#99999")
            .expect("upsert should succeed");

        let (_shared, tx, _client_writer, handle) =
            start_test_loop_with_snapshot_store(snapshot_store);

        let _ = tx.send(StateEvent::NetworkConnectivityChanged { online: false });
        let _ = tx.send(StateEvent::SnapshotHostReconnectResult {
            profile_name: "test-profile".to_string(),
            authority_node_id: "peer#99999".to_string(),
            result: Box::new(Err("profile not found".to_string())),
        });

        std::thread::sleep(Duration::from_millis(50));

        let reloaded = OutboundConnectionSnapshotStore::new(&snapshot_path);
        let entries = reloaded.load().expect("load should succeed");
        assert_eq!(
            entries.len(),
            1,
            "snapshot should be kept when snapshot reconnect fails while control plane is offline"
        );
        assert_eq!(entries[0].authority_node_id, "peer#99999");

        drop(tx);
        handle.join().expect("state loop should exit cleanly");
        crate::infra::best_effort::remove_file(&snapshot_path);
    }

    #[test]
    fn inbound_connect_offline_keeps_connection_until_reconnect_failed() {
        let _guard = STATE_LOOP_TEST_LOCK.lock().unwrap();
        let (shared, tx, _client_writer, handle) = start_test_loop();

        shared.record_remote_node_connection(
            "peer#99999",
            RemoteNodeConnectionInfo {
                mode: RemoteNodeConnectionMode::InboundConnect,
                host: "10.0.0.1".to_string(),
                port: 7474,
                tls_pin_sha256: String::new(),
                profile_name: "test-profile".to_string(),
                server_can_reach_peer: false,
            },
        );

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
            agent_command_name: None,
            current_path: None,
            task_state: ManagedSessionTaskState::Input,
        };
        let target_id = record.address.qualified_target();
        let _ = tx.send(StateEvent::RemoteSessionCatalogUpdated {
            record: Box::new(record),
        });
        std::thread::sleep(Duration::from_millis(50));

        let _ = tx.send(StateEvent::RemoteNodeOffline {
            node_id: "peer#99999".to_string(),
        });
        std::thread::sleep(Duration::from_millis(50));

        {
            let guard = shared
                .sessions
                .sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let session = guard.get(&target_id).expect("session should exist");
            assert_eq!(
                session.availability,
                SessionAvailability::Offline,
                "inbound-connect offline should mark sessions offline"
            );
        }
        assert!(
            shared.remote_node_connection("peer#99999").is_some(),
            "inbound-connect node connection should be kept while waiting for reconnect"
        );

        let _ = tx.send(StateEvent::RemoteNodeReconnectFailed {
            node_id: "peer#99999".to_string(),
        });
        std::thread::sleep(Duration::from_millis(50));

        {
            let guard = shared
                .sessions
                .sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            assert!(
                !guard.contains_key(&target_id),
                "sessions should be removed after reconnect failed"
            );
        }
        assert!(
            shared.remote_node_connection("peer#99999").is_some(),
            "node connection should be kept after reconnect failed so a later reconnect can reuse it"
        );

        drop(tx);
        handle.join().expect("state loop should exit cleanly");
    }

    #[test]
    fn wrap_bracketed_paste_adds_markers() {
        let wrapped = wrap_bracketed_paste(b"line1\nline2");
        assert_eq!(
            wrapped, b"\x1b[200~line1\nline2\x1b[201~",
            "expected bracketed-paste start/end markers"
        );
    }
}
