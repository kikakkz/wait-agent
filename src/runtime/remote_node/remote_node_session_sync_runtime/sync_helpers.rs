use crate::cli::RemoteAuthorityTargetHostCommand;
use crate::cli::{prepend_global_network_args, RemoteNetworkConfig};
use crate::domain::agent_detector::SHELL_NAMES;
use crate::domain::session_catalog::{
    ManagedSessionRecord, ManagedSessionTaskState, SessionTransport,
};
use crate::infra::error_log::ERROR_LOG;
use crate::infra::published_target_store::PublishedTargetStore;
use crate::infra::remote_grpc_proto::v1::node_session_envelope::Body;
use crate::infra::remote_grpc_proto::v1::{
    NodeSessionEnvelope as GrpcNodeSessionEnvelope, RouteContext, TargetExited, TargetPublished,
};
use crate::infra::remote_grpc_transport::{
    OutboundNodeSessionRequest, RemoteNodeSessionHandle, RemoteNodeTransportEvent,
};
use crate::infra::remote_protocol::{
    ControlPlanePayload, ErrorPayload, NodeSessionChannel, ProtocolEnvelope,
    TargetPublicationAckPayload, TargetPublicationAckStatus,
};
use crate::infra::remote_transport_codec::{
    read_authority_transport_frame, write_control_plane_envelope, AuthorityTransportFrame,
};
use crate::infra::tmux::{
    EmbeddedTmuxBackend, TmuxChromeGateway, TmuxSessionGateway, TmuxSessionName, TmuxSocketName,
    TmuxWorkspaceHandle,
};
use crate::lifecycle::LifecycleError;
use crate::runtime::current_executable::current_waitagent_executable;
use crate::runtime::remote_authority_target_host_runtime::RemoteAuthorityTargetHostRuntime;
use crate::runtime::remote_authority_transport_runtime::RemoteAuthorityCommand;
use crate::runtime::remote_node_session_owner_runtime::live_authority_session_socket_path;
use crate::runtime::remote_node_session_runtime::{
    map_inbound_grpc_authority_event, map_outbound_grpc_envelope,
};
use crate::runtime::remote_node_transport_runtime::{read_client_hello, write_server_hello};
use crate::runtime::remote_target_publication_runtime::RemoteTargetPublicationRuntime;
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::{
    LocalSessionCatalog, LocalTargetExitObserver, OutboundRemoteNodeTransport,
    SessionSyncAuthorityHost, SessionSyncAuthorityManager, SessionSyncAuthorityPublicationGateway,
    LIVE_AUTHORITY_SERVER_ID, SESSION_SYNC_AUTHORITY_ID, WAITAGENT_ACTIVE_TARGET_OPTION,
};

const SOURCE_PUBLICATION_RETRY_INITIAL_DELAY: Duration = Duration::from_millis(250);
const SOURCE_PUBLICATION_RETRY_MAX_DELAY: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalCatalogChangeReason {
    LocalTargetExited { target_session_name: String },
    LocalRuntimeChanged,
}

impl LocalCatalogChangeReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::LocalTargetExited { .. } => "local-target-exited",
            Self::LocalRuntimeChanged => "local-runtime-changed",
        }
    }

    fn encode(&self) -> String {
        match self {
            Self::LocalTargetExited {
                target_session_name,
            } => format!("local-target-exited	{target_session_name}"),
            Self::LocalRuntimeChanged => "local-runtime-changed".to_string(),
        }
    }

    fn parse(value: &str) -> Option<Self> {
        let (reason, detail) = value.split_once('\t').unwrap_or((value, ""));
        match reason {
            "local-target-exited" if !detail.is_empty() => Some(Self::LocalTargetExited {
                target_session_name: detail.to_string(),
            }),
            "local-runtime-changed" => Some(Self::LocalRuntimeChanged),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SourcePublicationState {
    session: ManagedSessionRecord,
    exited: bool,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub(super) struct PendingSourcePublication {
    pub(super) target_id: String,
    pub(super) revision: u64,
    pub(super) envelope: GrpcNodeSessionEnvelope,
    pub(super) retry_attempt: u32,
    pub(super) next_retry_at: Option<Instant>,
}

#[derive(Debug, Default)]
struct SourcePublicationRecord {
    last_state: Option<SourcePublicationState>,
    latest_revision: u64,
    pending: Option<PendingSourcePublication>,
    acked_revision: u64,
}

#[derive(Debug, Default)]
pub(super) struct SourcePublicationTracker {
    records: HashMap<String, SourcePublicationRecord>,
    connected: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SourcePublicationAckOutcome {
    Cleared,
    Retained,
    Ignored,
}

impl SourcePublicationTracker {
    pub(super) fn new() -> Self {
        Self::default()
    }

    pub(super) fn on_connected(&mut self) {
        self.connected = true;
        for record in self.records.values_mut() {
            if let Some(pending) = record.pending.as_mut() {
                pending.next_retry_at = None;
            }
        }
    }

    pub(super) fn on_disconnected(&mut self) {
        self.connected = false;
    }

    pub(super) fn on_state_changed(
        &mut self,
        node_id: &str,
        session_instance_id: &str,
        next_message_id: &mut u64,
        session: &ManagedSessionRecord,
    ) -> Option<PendingSourcePublication> {
        self.on_state_changed_with_mode(
            node_id,
            session_instance_id,
            next_message_id,
            session,
            SourcePublicationMode::Delta,
        )
    }

    pub(super) fn on_baseline_state(
        &mut self,
        node_id: &str,
        session_instance_id: &str,
        next_message_id: &mut u64,
        session: &ManagedSessionRecord,
    ) -> PendingSourcePublication {
        self.on_state_changed_with_mode(
            node_id,
            session_instance_id,
            next_message_id,
            session,
            SourcePublicationMode::FullBaseline,
        )
        .expect("full baseline publication should always create a pending record")
    }

    fn on_state_changed_with_mode(
        &mut self,
        node_id: &str,
        session_instance_id: &str,
        next_message_id: &mut u64,
        session: &ManagedSessionRecord,
        mode: SourcePublicationMode,
    ) -> Option<PendingSourcePublication> {
        let target_id = format!("remote-peer:{node_id}:{}", session.address.session_id());
        let state = SourcePublicationState {
            session: session.clone(),
            exited: false,
        };
        let record = self.records.entry(target_id.clone()).or_default();
        if mode == SourcePublicationMode::Delta && record.last_state.as_ref() == Some(&state) {
            return None;
        }
        record.latest_revision += 1;
        record.last_state = Some(state);
        next_message_id_increment(next_message_id);
        let mut envelope = remote_session_published_envelope(
            node_id,
            session_instance_id,
            *next_message_id,
            session,
        );
        if let Some(Body::TargetPublished(payload)) = envelope.body.as_mut() {
            payload.node_instance_id = session_instance_id.to_string();
            payload.revision = record.latest_revision;
        }
        let pending = PendingSourcePublication {
            target_id,
            revision: record.latest_revision,
            envelope,
            retry_attempt: 0,
            next_retry_at: None,
        };
        record.pending = Some(pending.clone());
        Some(pending)
    }

    pub(super) fn on_target_exited(
        &mut self,
        node_id: &str,
        session_instance_id: &str,
        next_message_id: &mut u64,
        transport_session_id: &str,
    ) -> PendingSourcePublication {
        let target_id = format!("remote-peer:{node_id}:{transport_session_id}");
        let record = self.records.entry(target_id.clone()).or_default();
        record.latest_revision += 1;
        record.last_state = Some(SourcePublicationState {
            session: exited_source_publication_state(node_id, transport_session_id),
            exited: true,
        });
        next_message_id_increment(next_message_id);
        let mut envelope = remote_session_exited_envelope(
            node_id,
            session_instance_id,
            *next_message_id,
            transport_session_id,
        );
        if let Some(Body::TargetExited(payload)) = envelope.body.as_mut() {
            payload.node_instance_id = session_instance_id.to_string();
            payload.revision = record.latest_revision;
        }
        let pending = PendingSourcePublication {
            target_id,
            revision: record.latest_revision,
            envelope,
            retry_attempt: 0,
            next_retry_at: None,
        };
        record.pending = Some(pending.clone());
        pending
    }

    pub(super) fn on_ack(
        &mut self,
        ack: &TargetPublicationAckPayload,
    ) -> SourcePublicationAckOutcome {
        let Some(record) = self.records.get_mut(&ack.target_id) else {
            return SourcePublicationAckOutcome::Ignored;
        };
        let Some(pending) = record.pending.as_ref() else {
            return SourcePublicationAckOutcome::Ignored;
        };
        if pending.revision != ack.revision {
            return SourcePublicationAckOutcome::Ignored;
        }
        match ack.status {
            TargetPublicationAckStatus::Applied | TargetPublicationAckStatus::StaleRevision => {
                record.acked_revision = ack.revision;
                record.pending = None;
                SourcePublicationAckOutcome::Cleared
            }
            TargetPublicationAckStatus::Failed => SourcePublicationAckOutcome::Retained,
        }
    }

    pub(super) fn on_publication_sent(&mut self, target_id: &str, revision: u64, now: Instant) {
        let Some(record) = self.records.get_mut(target_id) else {
            return;
        };
        let Some(pending) = record.pending.as_mut() else {
            return;
        };
        if pending.revision != revision {
            return;
        }
        pending.retry_attempt = pending.retry_attempt.saturating_add(1);
        pending.next_retry_at = Some(now + source_publication_retry_delay(pending.retry_attempt));
    }

    pub(super) fn is_current_pending(&self, target_id: &str, revision: u64) -> bool {
        self.records
            .get(target_id)
            .and_then(|record| record.pending.as_ref())
            .is_some_and(|pending| pending.revision == revision)
    }

    pub(super) fn due_retry_publications(&self, now: Instant) -> Vec<PendingSourcePublication> {
        if !self.connected {
            return Vec::new();
        }
        self.records
            .values()
            .filter_map(|record| {
                record
                    .pending
                    .as_ref()
                    .filter(|pending| {
                        pending
                            .next_retry_at
                            .is_some_and(|retry_at| retry_at <= now)
                    })
                    .cloned()
            })
            .collect()
    }

    pub(super) fn next_retry_delay(&self, now: Instant) -> Option<Duration> {
        if !self.connected {
            return None;
        }
        self.records
            .values()
            .filter_map(|record| record.pending.as_ref())
            .filter_map(|pending| {
                pending
                    .next_retry_at
                    .map(|retry_at| retry_at.saturating_duration_since(now))
            })
            .min()
    }

    #[allow(dead_code)]
    pub(super) fn pending_publications(&self) -> Vec<PendingSourcePublication> {
        if !self.connected {
            return Vec::new();
        }
        self.records
            .values()
            .filter_map(|record| record.pending.clone())
            .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourcePublicationMode {
    Delta,
    FullBaseline,
}

fn source_publication_retry_delay(attempt: u32) -> Duration {
    let factor = 1_u32
        .checked_shl(attempt.saturating_sub(1))
        .unwrap_or(u32::MAX);
    SOURCE_PUBLICATION_RETRY_INITIAL_DELAY
        .saturating_mul(factor)
        .min(SOURCE_PUBLICATION_RETRY_MAX_DELAY)
}

fn exited_source_publication_state(
    node_id: &str,
    transport_session_id: &str,
) -> ManagedSessionRecord {
    ManagedSessionRecord {
        address: crate::domain::session_catalog::ManagedSessionAddress::remote_peer(
            node_id,
            transport_session_id.to_string(),
        ),
        selector: None,
        availability: crate::domain::session_catalog::SessionAvailability::Exited,
        workspace_dir: None,
        workspace_key: None,
        session_role: None,
        opened_by: Vec::new(),
        attached_clients: 0,
        window_count: 0,
        command_name: None,
        current_path: None,
        task_state: ManagedSessionTaskState::Unknown,
    }
}

#[derive(Debug)]
pub(super) enum SessionSyncEvent {
    Transport(RemoteNodeTransportEvent),
    LocalCatalogChanged(LocalCatalogChangeReason),
    RetryDue,
    Stop,
}

pub(super) fn run_remote_session_sync_loop<G, T, O>(
    gateway: G,
    transport: T,
    network: RemoteNetworkConfig,
    local_target_exit_observer: O,
    node_id: String,
    endpoint_uri: String,
    local_catalog_rx: mpsc::Receiver<LocalCatalogChangeReason>,
    reconnect_delay: Duration,
    stop_rx: mpsc::Receiver<()>,
) where
    G: LocalSessionCatalog,
    T: OutboundRemoteNodeTransport,
    O: LocalTargetExitObserver,
{
    let publication_runtime =
        match RemoteTargetPublicationRuntime::from_build_env_with_network(network.clone()) {
            Ok(rt) => Some(rt),
            Err(error) => {
                ERROR_LOG.log(format!(
                    "[diag-sync] failed to create publication runtime: {error}"
                ));
                None
            }
        };
    let local_target_socket_name = gateway.local_target_socket_name().map(str::to_string);
    let mut next_message_id = 0_u64;
    let mut publication_tracker = SourcePublicationTracker::new();
    let mut synced_sessions = HashMap::<String, ManagedSessionRecord>::new();
    let mut pending_local_catalog_sync = false;
    let (session_event_tx, session_event_rx) = mpsc::channel::<SessionSyncEvent>();
    {
        let session_event_tx = session_event_tx.clone();
        thread::spawn(move || {
            while let Ok(reason) = local_catalog_rx.recv() {
                if session_event_tx
                    .send(SessionSyncEvent::LocalCatalogChanged(reason))
                    .is_err()
                {
                    return;
                }
            }
        });
    }
    {
        let session_event_tx = session_event_tx.clone();
        thread::spawn(move || {
            if stop_rx.recv().is_ok() {
                let _ = session_event_tx.send(SessionSyncEvent::Stop);
            }
        });
    }
    loop {
        let (transport_event_tx, transport_event_rx) = mpsc::channel();
        let _transport_guard = match transport.connect_outbound(
            OutboundNodeSessionRequest {
                node_id: node_id.clone(),
                endpoint_uri: endpoint_uri.clone(),
            },
            transport_event_tx,
        ) {
            Ok(guard) => guard,
            Err(_) => {
                publication_tracker.on_disconnected();
                match wait_for_reconnect_delay_or_stop(&session_event_rx, reconnect_delay) {
                    ReconnectWaitOutcome::Stop => return,
                    ReconnectWaitOutcome::LocalCatalogChanged => {
                        pending_local_catalog_sync = true;
                    }
                    ReconnectWaitOutcome::Elapsed => {}
                }
                continue;
            }
        };
        {
            let session_event_tx = session_event_tx.clone();
            thread::spawn(move || {
                while let Ok(event) = transport_event_rx.recv() {
                    if session_event_tx
                        .send(SessionSyncEvent::Transport(event))
                        .is_err()
                    {
                        return;
                    }
                }
            });
        }

        let mut active_session = None;
        let mut authority_manager =
            SessionSyncAuthorityManager::new(network.clone(), local_target_socket_name.clone());
        let mut should_reconnect = false;

        while !should_reconnect {
            let event = match recv_session_sync_event(
                &session_event_rx,
                publication_tracker.next_retry_delay(Instant::now()),
            ) {
                Some(event) => event,
                None => return,
            };

            match event {
                SessionSyncEvent::Transport(event) => {
                    let session_opened =
                        matches!(event, RemoteNodeTransportEvent::SessionOpened { .. });
                    let outcome = handle_transport_event(
                        event,
                        &mut active_session,
                        &mut authority_manager,
                        publication_runtime.as_ref(),
                        &node_id,
                    );
                    should_reconnect |= outcome.should_reconnect;
                    if outcome.should_reconnect {
                        publication_tracker.on_disconnected();
                    }
                    handle_publication_ack_outcome(
                        &mut publication_tracker,
                        outcome.publication_ack.as_ref(),
                    );
                    if outcome.local_catalog_changed {
                        should_reconnect |= sync_local_sessions_after_catalog_transport_event(
                            &gateway,
                            &node_id,
                            active_session.as_ref(),
                            &local_target_exit_observer,
                            &mut synced_sessions,
                            &mut next_message_id,
                            &mut publication_tracker,
                            "local catalog transport event",
                        );
                    }
                    if session_opened {
                        should_reconnect |= sync_after_session_opened(
                            &gateway,
                            &node_id,
                            active_session.as_ref(),
                            &local_target_exit_observer,
                            &mut synced_sessions,
                            &mut next_message_id,
                            &mut publication_tracker,
                            "SessionOpened",
                        );
                        if !should_reconnect && pending_local_catalog_sync {
                            ERROR_LOG.log(
                                "[diag-sync] replaying pending local catalog change after SessionOpened"
                                    .to_string(),
                            );
                            should_reconnect |= sync_local_sessions_after_catalog_transport_event(
                                &gateway,
                                &node_id,
                                active_session.as_ref(),
                                &local_target_exit_observer,
                                &mut synced_sessions,
                                &mut next_message_id,
                                &mut publication_tracker,
                                "pending local catalog change",
                            );
                            if !should_reconnect {
                                pending_local_catalog_sync = false;
                            }
                        }
                    }
                }
                SessionSyncEvent::RetryDue => {
                    if let Some(session_handle) = active_session.as_ref() {
                        let due = publication_tracker.due_retry_publications(Instant::now());
                        if !due.is_empty() {
                            ERROR_LOG.log(format!(
                                "[diag-publication] retrying pending source publications count={}",
                                due.len()
                            ));
                        }
                        if let Err(error) = send_pending_source_publications(
                            session_handle,
                            &mut publication_tracker,
                            due,
                        ) {
                            ERROR_LOG.log(format!(
                                "[diag-publication] source publication retry failed, will reconnect: {error}"
                            ));
                            publication_tracker.on_disconnected();
                            should_reconnect = true;
                        }
                    }
                }
                SessionSyncEvent::LocalCatalogChanged(reason) => {
                    ERROR_LOG.log_exit_latency(format!(
                        "[diag-exit] sync_event_received reason={} stage=session_sync",
                        reason.as_str()
                    ));
                    if let Some(session_handle) = active_session.as_ref() {
                        if let Err(_) = sync_local_sessions(
                            &gateway,
                            &node_id,
                            session_handle,
                            &local_target_exit_observer,
                            &mut synced_sessions,
                            &mut next_message_id,
                            &mut publication_tracker,
                            SessionSyncMode::Delta,
                        ) {
                            ERROR_LOG.log(
                                "[diag-sync] sync_local_sessions after local catalog change failed, will reconnect"
                                    .to_string(),
                            );
                            should_reconnect = true;
                        }
                    } else {
                        pending_local_catalog_sync = true;
                    }
                }
                SessionSyncEvent::Stop => return,
            }
        }

        match wait_for_reconnect_delay_or_stop(&session_event_rx, reconnect_delay) {
            ReconnectWaitOutcome::Stop => return,
            ReconnectWaitOutcome::LocalCatalogChanged => {
                pending_local_catalog_sync = true;
            }
            ReconnectWaitOutcome::Elapsed => {}
        }
        authority_manager.shutdown();
    }
}

fn sync_after_session_opened<G, O>(
    gateway: &G,
    node_id: &str,
    session_handle: Option<&RemoteNodeSessionHandle>,
    local_target_exit_observer: &O,
    synced_sessions: &mut HashMap<String, ManagedSessionRecord>,
    next_message_id: &mut u64,
    publication_tracker: &mut SourcePublicationTracker,
    reason: &str,
) -> bool
where
    G: LocalSessionCatalog,
    O: LocalTargetExitObserver,
{
    publication_tracker.on_connected();
    let Some(session_handle) = session_handle else {
        return false;
    };
    if let Err(_) = sync_local_sessions(
        gateway,
        node_id,
        session_handle,
        local_target_exit_observer,
        synced_sessions,
        next_message_id,
        publication_tracker,
        SessionSyncMode::FullBaseline,
    ) {
        ERROR_LOG.log(format!(
            "[diag-sync] sync_local_sessions after {reason} failed, will reconnect"
        ));
        publication_tracker.on_disconnected();
        return true;
    }
    false
}

fn handle_publication_ack_outcome(
    tracker: &mut SourcePublicationTracker,
    ack: Option<&TargetPublicationAckPayload>,
) {
    let Some(ack) = ack else {
        return;
    };
    match tracker.on_ack(ack) {
        SourcePublicationAckOutcome::Cleared => ERROR_LOG.log(format!(
            "[diag-publication] source publication ack cleared target={} revision={}",
            ack.target_id, ack.revision
        )),
        SourcePublicationAckOutcome::Retained => ERROR_LOG.log(format!(
            "[diag-publication] source publication ack failed target={} revision={}",
            ack.target_id, ack.revision
        )),
        SourcePublicationAckOutcome::Ignored => {}
    }
}

pub(super) fn sync_local_sessions_after_catalog_transport_event<G, O>(
    gateway: &G,
    node_id: &str,
    session_handle: Option<&RemoteNodeSessionHandle>,
    local_target_exit_observer: &O,
    synced_sessions: &mut HashMap<String, ManagedSessionRecord>,
    next_message_id: &mut u64,
    publication_tracker: &mut SourcePublicationTracker,
    reason: &str,
) -> bool
where
    G: LocalSessionCatalog,
    O: LocalTargetExitObserver,
{
    let Some(session_handle) = session_handle else {
        return false;
    };
    if let Err(_) = sync_local_sessions(
        gateway,
        node_id,
        session_handle,
        local_target_exit_observer,
        synced_sessions,
        next_message_id,
        publication_tracker,
        SessionSyncMode::Delta,
    ) {
        ERROR_LOG.log(format!(
            "[diag-sync] sync_local_sessions after {reason} failed, will reconnect"
        ));
        publication_tracker.on_disconnected();
        return true;
    }
    false
}

fn recv_session_sync_event(
    session_event_rx: &mpsc::Receiver<SessionSyncEvent>,
    retry_delay: Option<Duration>,
) -> Option<SessionSyncEvent> {
    match retry_delay {
        Some(delay) => match session_event_rx.recv_timeout(delay) {
            Ok(event) => Some(event),
            Err(mpsc::RecvTimeoutError::Timeout) => Some(SessionSyncEvent::RetryDue),
            Err(mpsc::RecvTimeoutError::Disconnected) => None,
        },
        None => session_event_rx.recv().ok(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ReconnectWaitOutcome {
    Elapsed,
    LocalCatalogChanged,
    Stop,
}

pub(super) fn wait_for_reconnect_delay_or_stop(
    session_event_rx: &mpsc::Receiver<SessionSyncEvent>,
    duration: Duration,
) -> ReconnectWaitOutcome {
    let deadline = Instant::now() + duration;
    let mut saw_local_catalog_change = false;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return if saw_local_catalog_change {
                ReconnectWaitOutcome::LocalCatalogChanged
            } else {
                ReconnectWaitOutcome::Elapsed
            };
        }
        match session_event_rx.recv_timeout(remaining) {
            Ok(SessionSyncEvent::Stop) => return ReconnectWaitOutcome::Stop,
            Ok(SessionSyncEvent::LocalCatalogChanged(reason)) => {
                ERROR_LOG.log_exit_latency(format!(
                    "[diag-exit] sync_event_queued_for_reconnect reason={} stage=session_sync",
                    reason.as_str()
                ));
                saw_local_catalog_change = true;
            }
            Ok(_) => continue,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                return if saw_local_catalog_change {
                    ReconnectWaitOutcome::LocalCatalogChanged
                } else {
                    ReconnectWaitOutcome::Elapsed
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return ReconnectWaitOutcome::Stop,
        }
    }
}

#[derive(Debug, Default)]
pub(super) struct TransportEventOutcome {
    pub(super) should_reconnect: bool,
    pub(super) local_catalog_changed: bool,
    pub(super) publication_ack: Option<TargetPublicationAckPayload>,
}

pub(super) fn handle_transport_event(
    event: RemoteNodeTransportEvent,
    active_session: &mut Option<RemoteNodeSessionHandle>,
    authority_manager: &mut SessionSyncAuthorityManager,
    publication_runtime: Option<&RemoteTargetPublicationRuntime>,
    node_id: &str,
) -> TransportEventOutcome {
    match event {
        RemoteNodeTransportEvent::SessionOpened { session } => {
            *active_session = Some(session);
            TransportEventOutcome::default()
        }
        RemoteNodeTransportEvent::EnvelopeReceived { envelope, .. } => {
            let Some(session_handle) = active_session.as_ref() else {
                return TransportEventOutcome::default();
            };
            if let Some(event) = map_inbound_grpc_authority_event(envelope) {
                if let crate::runtime::remote_node_session_runtime::GrpcAuthorityEvent::TargetPublicationAck(payload) = event {
                    return TransportEventOutcome {
                        publication_ack: Some(payload),
                        ..TransportEventOutcome::default()
                    };
                }
                return TransportEventOutcome {
                    should_reconnect: false,
                    local_catalog_changed: authority_manager.handle_event(session_handle, event),
                    ..TransportEventOutcome::default()
                };
            }
            TransportEventOutcome::default()
        }
        RemoteNodeTransportEvent::SessionClosed {
            node_id: event_node_id,
            ..
        } => {
            let node_id = event_node_id.as_str();
            ERROR_LOG.log(format!(
                "[diag-sync] SessionClosed for node {node_id}, will reconnect"
            ));
            if let Some(publication_runtime) = publication_runtime {
                mark_discovered_remote_node_offline_best_effort(publication_runtime, node_id);
            }
            authority_manager.shutdown();
            *active_session = None;
            TransportEventOutcome {
                should_reconnect: true,
                ..TransportEventOutcome::default()
            }
        }
        RemoteNodeTransportEvent::TransportFailed {
            node_id: event_node_id,
            message,
            ..
        } => {
            let node_id = event_node_id.as_deref().unwrap_or(node_id);
            ERROR_LOG.log(format!(
                "[diag-sync] TransportFailed node={node_id} msg={message}, will reconnect"
            ));
            if let Some(publication_runtime) = publication_runtime {
                mark_discovered_remote_node_offline_best_effort(publication_runtime, node_id);
            }
            authority_manager.shutdown();
            *active_session = None;
            TransportEventOutcome {
                should_reconnect: true,
                ..TransportEventOutcome::default()
            }
        }
    }
}

fn mark_discovered_remote_node_offline_best_effort(
    publication_runtime: &RemoteTargetPublicationRuntime,
    node_id: &str,
) {
    let publication_runtime = publication_runtime.clone();
    let node_id = node_id.to_string();
    thread::spawn(move || {
        if let Err(error) = publication_runtime.mark_discovered_remote_node_offline(&node_id) {
            ERROR_LOG.log(format!(
                "[diag-sync] failed to mark discovered remote node offline {node_id}: {error}"
            ));
        }
    });
}

fn send_source_publication(
    session_handle: &RemoteNodeSessionHandle,
    tracker: &mut SourcePublicationTracker,
    publication: &PendingSourcePublication,
) -> Result<(), crate::infra::remote_grpc_transport::RemoteNodeTransportError> {
    if !tracker.is_current_pending(&publication.target_id, publication.revision) {
        return Ok(());
    }
    session_handle.send(publication.envelope.clone())?;
    tracker.on_publication_sent(&publication.target_id, publication.revision, Instant::now());
    Ok(())
}

fn send_pending_source_publications(
    session_handle: &RemoteNodeSessionHandle,
    tracker: &mut SourcePublicationTracker,
    publications: Vec<PendingSourcePublication>,
) -> Result<(), crate::infra::remote_grpc_transport::RemoteNodeTransportError> {
    for publication in publications {
        send_source_publication(session_handle, tracker, &publication)?;
    }
    Ok(())
}

pub(super) fn sync_local_sessions<G, O>(
    gateway: &G,
    node_id: &str,
    session_handle: &RemoteNodeSessionHandle,
    local_target_exit_observer: &O,
    synced_sessions: &mut HashMap<String, ManagedSessionRecord>,
    next_message_id: &mut u64,
    publication_tracker: &mut SourcePublicationTracker,
    mode: SessionSyncMode,
) -> Result<(), io::Error>
where
    G: LocalSessionCatalog,
    O: LocalTargetExitObserver,
{
    let t_sync = Instant::now();
    ERROR_LOG.log(format!(
        "[diag-newhost] sync_local_sessions start node={}",
        node_id
    ));
    let local_sessions = match gateway.list_local_sessions() {
        Ok(sessions) => {
            ERROR_LOG.log(format!(
                "[diag-timing] sync_local_sessions: found {} local sessions",
                sessions.len()
            ));
            ERROR_LOG.log(format!(
                "[diag-newhost] sync_local_sessions list_local_sessions node={} sessions={} elapsed={:?}",
                node_id,
                sessions.len(),
                t_sync.elapsed()
            ));
            sessions
        }
        Err(_) => {
            ERROR_LOG
                .log("[diag-timing] sync_local_sessions: list_local_sessions FAILED".to_string());
            ERROR_LOG.log(format!(
                "[diag-newhost] sync_local_sessions list_local_sessions FAILED node={} elapsed={:?}",
                node_id,
                t_sync.elapsed()
            ));
            return Ok(());
        }
    };
    let local_sessions: Vec<ManagedSessionRecord> = local_sessions
        .into_iter()
        .filter(|s| *s.address.transport() == SessionTransport::LocalTmux)
        .collect();
    ERROR_LOG.log(format!(
        "[diag-timing] sync_local_sessions: after filter {} local sessions",
        local_sessions.len()
    ));
    ERROR_LOG.log(format!(
        "[diag-newhost] sync_local_sessions filter node={} local_sessions={} elapsed={:?}",
        node_id,
        local_sessions.len(),
        t_sync.elapsed()
    ));
    let current_sessions = local_sessions_by_local_id(local_sessions);
    let delta = compute_session_sync_delta(synced_sessions, &current_sessions, mode);
    ERROR_LOG.log(format!(
        "[diag-newhost] sync_local_sessions delta node={} publish={} exit={} elapsed={:?}",
        node_id,
        delta.publish.len(),
        delta.exit.len(),
        t_sync.elapsed()
    ));
    for session in &delta.publish {
        ERROR_LOG.log(format!(
            "[diag-sync] publishing target node={} target={}",
            node_id,
            session.address.qualified_target()
        ));
    }
    ERROR_LOG.log(format!(
        "[diag-timing] sync_local_sessions: delta publish={} exit={}",
        delta.publish.len(),
        delta.exit.len()
    ));

    for session in &delta.publish {
        let t_send = Instant::now();
        let publication = match mode {
            SessionSyncMode::Delta => {
                let Some(publication) = publication_tracker.on_state_changed(
                    node_id,
                    session_handle.session_instance_id(),
                    next_message_id,
                    session,
                ) else {
                    continue;
                };
                publication
            }
            SessionSyncMode::FullBaseline => publication_tracker.on_baseline_state(
                node_id,
                session_handle.session_instance_id(),
                next_message_id,
                session,
            ),
        };
        if let Err(error) =
            send_source_publication(session_handle, publication_tracker, &publication)
        {
            ERROR_LOG.log(format!("[diag-sync] session_handle.send failed: {error}"));
            ERROR_LOG.log(format!(
                "[diag-newhost] sync_local_sessions publish_send FAILED node={} target={} elapsed={:?} total={:?}",
                node_id,
                session.address.qualified_target(),
                t_send.elapsed(),
                t_sync.elapsed()
            ));
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, error.to_string()));
        }
        ERROR_LOG.log(format!(
            "[diag-newhost] sync_local_sessions publish_send node={} target={} elapsed={:?} total={:?}",
            node_id,
            session.address.qualified_target(),
            t_send.elapsed(),
            t_sync.elapsed()
        ));
    }

    for previous in &delta.exit {
        let t_exit = Instant::now();
        ERROR_LOG.log_exit_latency(format!(
            "[diag-exit] sync_exit_start node={} target={} total={:?} stage=session_sync",
            node_id,
            previous.address.qualified_target(),
            t_sync.elapsed()
        ));
        if previous.is_target_host() {
            let t_observe = Instant::now();
            if let Err(error) = local_target_exit_observer.observe_local_target_exit(
                previous.address.server_id(),
                previous.address.session_id(),
            ) {
                ERROR_LOG.log(format!(
                    "[diag-sync] failed to observe local target exit socket={} target={}: {error}",
                    previous.address.server_id(),
                    previous.address.session_id()
                ));
            }
            ERROR_LOG.log_exit_latency(format!(
                "[diag-exit] sync_exit_observe_local node={} target={} elapsed={:?} total={:?} stage=session_sync",
                node_id,
                previous.address.qualified_target(),
                t_observe.elapsed(),
                t_sync.elapsed()
            ));
        }
        let t_send = Instant::now();
        let publication = publication_tracker.on_target_exited(
            node_id,
            session_handle.session_instance_id(),
            next_message_id,
            previous.address.session_id(),
        );
        send_source_publication(session_handle, publication_tracker, &publication)
            .map_err(|error| io::Error::new(io::ErrorKind::BrokenPipe, error.to_string()))?;
        ERROR_LOG.log_exit_latency(format!(
            "[diag-exit] sync_exit_send node={} target={} elapsed={:?} total={:?} exit_total={:?} stage=session_sync",
            node_id,
            previous.address.qualified_target(),
            t_send.elapsed(),
            t_sync.elapsed(),
            t_exit.elapsed()
        ));
    }

    *synced_sessions = current_sessions;
    ERROR_LOG.log(format!(
        "[diag-newhost] sync_local_sessions done node={} elapsed={:?}",
        node_id,
        t_sync.elapsed()
    ));
    Ok(())
}

pub(crate) fn exportable_local_sessions_for_socket(
    sessions: Vec<ManagedSessionRecord>,
    socket_name: &str,
    published_target_store: &PublishedTargetStore,
) -> Vec<ManagedSessionRecord> {
    sessions
        .into_iter()
        .filter(|session| {
            session.address.server_id() == socket_name
                && session.is_workspace_session()
                && session.availability
                    != crate::domain::session_catalog::SessionAvailability::Exited
        })
        .map(|session| {
            exported_session_record_for_socket(session, socket_name, published_target_store)
        })
        .collect()
}

fn exported_session_record_for_socket(
    session: ManagedSessionRecord,
    socket_name: &str,
    published_target_store: &PublishedTargetStore,
) -> ManagedSessionRecord {
    if !session.is_target_host() {
        return session;
    }
    let Ok(records) = published_target_store
        .list_records_for_source_binding(socket_name, session.address.session_id())
    else {
        return session;
    };
    records
        .into_iter()
        .find(|record| record.target.is_target_host())
        .map(|record| {
            merge_cached_remote_identity_with_live_target_runtime(record.target, &session)
        })
        .unwrap_or(session)
}

fn merge_cached_remote_identity_with_live_target_runtime(
    mut cached_remote_target: ManagedSessionRecord,
    live_target: &ManagedSessionRecord,
) -> ManagedSessionRecord {
    cached_remote_target.availability = live_target.availability;
    cached_remote_target.workspace_key = live_target.workspace_key.clone();
    cached_remote_target.session_role = live_target.session_role;
    cached_remote_target.attached_clients = live_target.attached_clients;
    cached_remote_target.window_count = live_target.window_count;
    if !live_target_uses_internal_waitagent_runtime(live_target) {
        cached_remote_target.command_name = live_target.command_name.clone();
        cached_remote_target.current_path = live_target.current_path.clone();
        cached_remote_target.task_state = live_target.task_state;
    }
    cached_remote_target
}

pub(super) fn active_workspace_targets_on_socket<G>(
    gateway: &G,
    socket_name: &TmuxSocketName,
    sessions: &[ManagedSessionRecord],
) -> Result<HashMap<String, String>, G::Error>
where
    G: TmuxChromeGateway,
{
    let mut active_targets = HashMap::new();
    for session in sessions
        .iter()
        .filter(|session| session.is_workspace_chrome())
    {
        let workspace = TmuxWorkspaceHandle {
            workspace_id: crate::domain::workspace::WorkspaceInstanceId::new(
                session.address.session_id(),
            ),
            socket_name: socket_name.clone(),
            session_name: TmuxSessionName::new(session.address.session_id()),
        };
        if let Some(active_target) = gateway
            .show_session_option(&workspace, WAITAGENT_ACTIVE_TARGET_OPTION)?
            .filter(|target| !target.is_empty())
        {
            active_targets.insert(session.address.session_id().to_string(), active_target);
        }
    }
    Ok(active_targets)
}

pub(crate) fn overlay_workspace_runtime_onto_active_local_target_hosts(
    sessions: Vec<ManagedSessionRecord>,
    socket_name: &str,
    active_targets: &HashMap<String, String>,
) -> Vec<ManagedSessionRecord> {
    let workspace_runtimes = sessions
        .iter()
        .filter(|session| session.is_workspace_chrome())
        .cloned()
        .collect::<Vec<_>>();
    let mut sessions = sessions;
    for workspace_runtime in workspace_runtimes {
        let Some(active_target) = active_targets.get(workspace_runtime.address.session_id()) else {
            continue;
        };
        let Some(active_target) = sessions.iter_mut().find(|session| {
            session.address.server_id() == socket_name
                && session.is_target_host()
                && session.address.qualified_target() == *active_target
        }) else {
            continue;
        };
        if should_overlay_active_target_runtime(active_target, &workspace_runtime) {
            active_target.command_name = workspace_runtime.command_name.clone();
            active_target.current_path = workspace_runtime.current_path.clone();
            active_target.task_state = workspace_runtime.task_state;
        }
    }
    sessions
}

fn should_overlay_active_target_runtime(
    session: &ManagedSessionRecord,
    workspace_runtime: &ManagedSessionRecord,
) -> bool {
    if session_has_explicit_runtime(session) {
        return false;
    }
    workspace_runtime
        .command_name
        .as_deref()
        .is_some_and(|name| name != "waitagent")
        && session
            .command_name
            .as_deref()
            .map_or(true, |name| SHELL_NAMES.contains(&name))
        && matches!(
            session.task_state,
            ManagedSessionTaskState::Unknown
                | ManagedSessionTaskState::Running
                | ManagedSessionTaskState::Input
        )
}

fn session_has_explicit_runtime(session: &ManagedSessionRecord) -> bool {
    session
        .command_name
        .as_deref()
        .is_some_and(|name| !name.is_empty())
        && matches!(
            session.task_state,
            ManagedSessionTaskState::Input
                | ManagedSessionTaskState::Running
                | ManagedSessionTaskState::Confirm
        )
}

pub(crate) fn local_sessions_by_local_id(
    sessions: Vec<ManagedSessionRecord>,
) -> HashMap<String, ManagedSessionRecord> {
    sessions
        .into_iter()
        .map(|session| (session.address.id().as_str().to_string(), session))
        .collect()
}

fn live_target_uses_internal_waitagent_runtime(session: &ManagedSessionRecord) -> bool {
    session.command_name.as_deref() == Some("waitagent")
}

#[derive(Debug)]
pub(crate) struct SessionSyncDelta {
    pub(crate) publish: Vec<ManagedSessionRecord>,
    pub(crate) exit: Vec<ManagedSessionRecord>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionSyncMode {
    Delta,
    FullBaseline,
}

pub(crate) fn compute_session_sync_delta(
    previous: &HashMap<String, ManagedSessionRecord>,
    current: &HashMap<String, ManagedSessionRecord>,
    mode: SessionSyncMode,
) -> SessionSyncDelta {
    let publish = current
        .iter()
        .filter_map(|(local_id, session)| {
            if mode == SessionSyncMode::Delta
                && previous
                    .get(local_id)
                    .is_some_and(|previous| session_records_equivalent_for_sync(previous, session))
            {
                None
            } else {
                Some(session.clone())
            }
        })
        .collect::<Vec<_>>();
    let exit = previous
        .iter()
        .filter_map(|(local_id, session)| {
            if current.contains_key(local_id) {
                None
            } else {
                Some(session.clone())
            }
        })
        .collect::<Vec<_>>();
    SessionSyncDelta { publish, exit }
}

fn session_records_equivalent_for_sync(
    previous: &ManagedSessionRecord,
    current: &ManagedSessionRecord,
) -> bool {
    let mut previous = previous.clone();
    let mut current = current.clone();
    if is_interactive_shell_state(&previous) && is_interactive_shell_state(&current) {
        // Normalize Running/Input to Input only when the state hasn't
        // actually changed.  This prevents spurious publications from
        // prompt-character fluctuations (e.g. `$` vs `%` between polls),
        // while still publishing a meaningful Running→Input transition
        // (e.g. a command finished and the prompt just appeared).
        if previous.task_state == current.task_state {
            previous.task_state = ManagedSessionTaskState::Input;
            current.task_state = ManagedSessionTaskState::Input;
        }
    }
    previous == current
}

fn is_interactive_shell_state(session: &ManagedSessionRecord) -> bool {
    session
        .command_name
        .as_deref()
        .is_some_and(|name| SHELL_NAMES.contains(&name))
        && matches!(
            session.task_state,
            ManagedSessionTaskState::Input | ManagedSessionTaskState::Running
        )
}

pub(super) fn authority_command_target_id(command: &RemoteAuthorityCommand) -> &str {
    match command {
        RemoteAuthorityCommand::OpenMirror(payload) => payload.target_id.as_str(),
        RemoteAuthorityCommand::CloseMirror(payload) => payload.target_id.as_str(),
        RemoteAuthorityCommand::RawPtyInput(payload) => payload.target_id.as_str(),
        RemoteAuthorityCommand::ApplyResize(payload) => payload.target_id.as_str(),
        RemoteAuthorityCommand::SyncRequest { .. } => "",
    }
}

pub(super) fn remote_session_sync_error<E>(error: E) -> LifecycleError
where
    E: ToString,
{
    LifecycleError::Io(
        "failed to start remote session sync runtime".to_string(),
        io::Error::new(io::ErrorKind::Other, error.to_string()),
    )
}

pub(super) fn wait_for_live_authority_socket(socket_path: &Path) -> Result<(), LifecycleError> {
    for _ in 0..100 {
        if socket_path.exists() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(10));
    }
    Err(LifecycleError::Protocol(format!(
        "authority live-session socket did not become ready at {}",
        socket_path.display()
    )))
}

pub(super) fn target_session_name_from_target_id(target_id: &str) -> Option<String> {
    let target_id = target_id
        .strip_prefix("remote-peer:")
        .or_else(|| target_id.strip_prefix("local-tmux:"))
        .or_else(|| target_id.strip_prefix("remote:"))
        .unwrap_or(target_id);
    let (_, session_name) = target_id.rsplit_once(':')?;
    if session_name.is_empty() {
        None
    } else {
        Some(session_name.to_string())
    }
}

pub(super) fn find_socket_for_session(target_session_name: &str) -> Option<String> {
    let backend = EmbeddedTmuxBackend::from_build_env().ok()?;
    let sockets = backend.discover_waitagent_sockets().ok()?;
    for socket_name in &sockets {
        let sessions = backend.list_sessions_on_socket(socket_name).ok()?;
        if sessions
            .iter()
            .any(|s| s.address.session_id() == target_session_name)
        {
            return Some(socket_name.as_str().to_string());
        }
    }
    None
}

pub(super) fn spawn_in_process_authority_target_host(
    running: Arc<AtomicBool>,
    writer: Arc<Mutex<Option<UnixStream>>>,
    writer_ready: Arc<Condvar>,
    network: RemoteNetworkConfig,
    command: RemoteAuthorityTargetHostCommand,
) -> Result<(), LifecycleError> {
    let gateway = EmbeddedTmuxBackend::from_build_env().map_err(remote_session_sync_error)?;
    let current_executable = current_waitagent_executable()?;
    let runtime = RemoteAuthorityTargetHostRuntime::new(
        gateway,
        SessionSyncAuthorityPublicationGateway::new(network),
        current_executable,
    );
    let authority_socket_path =
        live_authority_session_socket_path(&command.socket_name, &command.target_session_name);
    thread::spawn(move || {
        let _ = runtime.run_target_host(command);
        running.store(false, Ordering::Relaxed);
        let writer_val = match writer.lock() {
            Ok(mut guard) => guard.take(),
            Err(poisoned) => {
                ERROR_LOG.log(
                    "[session-sync] authority writer mutex poisoned during host cleanup, recovering".to_string()
                );
                poisoned.into_inner().take()
            }
        };
        if let Some(writer) = writer_val {
            let _ = writer.shutdown(Shutdown::Both);
        }
        writer_ready.notify_all();
        let _ = UnixStream::connect(&authority_socket_path);
    });
    Ok(())
}

pub(super) fn spawn_live_authority_listener(
    socket_path: PathBuf,
    session_handle: RemoteNodeSessionHandle,
    running: Arc<AtomicBool>,
    writer: Arc<Mutex<Option<UnixStream>>>,
    writer_ready: Arc<Condvar>,
) {
    thread::spawn(move || {
        let Ok(listener) = bind_live_authority_listener(&socket_path) else {
            running.store(false, Ordering::Relaxed);
            return;
        };
        while running.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((stream, _)) => {
                    let _ = bridge_live_authority_stream(
                        stream,
                        session_handle.clone(),
                        running.clone(),
                        writer.clone(),
                        writer_ready.clone(),
                    );
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }
        let _ = std::fs::remove_file(&socket_path);
    });
}

fn bind_live_authority_listener(socket_path: &Path) -> Result<UnixListener, io::Error> {
    if socket_path.exists() {
        let _ = std::fs::remove_file(socket_path);
    }
    let listener = UnixListener::bind(socket_path)?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

fn bridge_live_authority_stream(
    mut host_stream: UnixStream,
    session_handle: RemoteNodeSessionHandle,
    running: Arc<AtomicBool>,
    writer: Arc<Mutex<Option<UnixStream>>>,
    writer_ready: Arc<Condvar>,
) -> Result<(), LifecycleError> {
    let _node_id = read_client_hello(&mut host_stream).map_err(remote_session_sync_error)?;
    write_server_hello(&mut host_stream, LIVE_AUTHORITY_SERVER_ID)
        .map_err(remote_session_sync_error)?;
    let host_reader = host_stream.try_clone().map_err(remote_session_sync_error)?;
    {
        let mut writer_guard = match writer.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                ERROR_LOG.log(
                    "[session-sync] authority writer mutex poisoned in bridge, recovering"
                        .to_string(),
                );
                poisoned.into_inner()
            }
        };
        if let Some(previous) = writer_guard.take() {
            let _ = previous.shutdown(Shutdown::Both);
        }
        *writer_guard = Some(host_stream.try_clone().map_err(remote_session_sync_error)?);
    }
    writer_ready.notify_all();
    ERROR_LOG.log("[diag-timing] bridge_live_authority_stream: writer set, ready signalled, starting forward_host_output_to_session".to_string());
    let result = forward_host_output_to_session(host_reader, session_handle, running.clone());
    ERROR_LOG.log(format!("[diag-timing] bridge_live_authority_stream: forward_host_output_to_session exited, result={:?}", result));
    let _ = host_stream.shutdown(Shutdown::Both);
    let _ = match writer.lock() {
        Ok(mut guard) => guard.take(),
        Err(poisoned) => {
            ERROR_LOG.log(
                "[session-sync] authority writer mutex poisoned in bridge cleanup, recovering"
                    .to_string(),
            );
            poisoned.into_inner().take()
        }
    };
    result
}

fn forward_host_output_to_session(
    mut host_reader: UnixStream,
    session_handle: RemoteNodeSessionHandle,
    running: Arc<AtomicBool>,
) -> Result<(), LifecycleError> {
    while running.load(Ordering::Relaxed) {
        let envelope = match read_authority_transport_frame(&mut host_reader) {
            Ok(AuthorityTransportFrame::ControlPlane(envelope)) => envelope,
            Ok(AuthorityTransportFrame::RawPtyOutput(payload)) => ProtocolEnvelope {
                protocol_version: crate::infra::remote_protocol::REMOTE_PROTOCOL_VERSION
                    .to_string(),
                message_id: format!(
                    "{}-raw-pty-output-{}",
                    session_handle.node_id(),
                    payload.output_seq
                ),
                message_type: "raw_pty_output",
                timestamp: String::new(),
                sender_id: session_handle.node_id().to_string(),
                correlation_id: None,
                session_id: Some(payload.session_id.clone()),
                target_id: Some(payload.target_id.clone()),
                attachment_id: None,
                console_id: None,
                payload: ControlPlanePayload::RawPtyOutput(payload),
            },
            Ok(other) => {
                ERROR_LOG.log(format!(
                    "[diag-timing] forward_host_output_to_session: unexpected authority frame {other:?}, exiting"
                ));
                return Err(remote_session_sync_error(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("unexpected authority frame {other:?}"),
                )));
            }
            Err(e) => {
                ERROR_LOG.log(format!(
                    "[diag-timing] forward_host_output_to_session: read_authority_transport_frame failed: {e}, exiting"
                ));
                return Err(remote_session_sync_error(e));
            }
        };
        let grpc = map_outbound_grpc_envelope(
            session_handle.node_id(),
            NodeSessionChannel::Authority,
            &envelope,
        )
        .map_err(remote_session_sync_error)?;
        ERROR_LOG.log(format!(
            "[diag-timing] forward_host_output_to_session: forwarding envelope type={} to gRPC",
            envelope.payload.message_type()
        ));
        if let Err(e) = session_handle.send(grpc) {
            ERROR_LOG.log(format!(
                "[diag-timing] forward_host_output_to_session: session_handle.send failed: {e}, exiting"
            ));
            return Err(remote_session_sync_error(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                e.to_string(),
            )));
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AuthorityHostSignal {
    Ready,
    Starting,
    Closed,
}

const AUTHORITY_HOST_READY_TIMEOUT: Duration = Duration::from_secs(5);

pub(super) fn authority_host_signal(host: &SessionSyncAuthorityHost) -> AuthorityHostSignal {
    match host.writer.lock() {
        Ok(guard) => {
            if guard.is_some() {
                AuthorityHostSignal::Ready
            } else if host.running.load(Ordering::Relaxed) {
                AuthorityHostSignal::Starting
            } else {
                AuthorityHostSignal::Closed
            }
        }
        Err(poisoned) => {
            ERROR_LOG.log("[session-sync] authority writer mutex poisoned, recovering".to_string());
            let guard = poisoned.into_inner();
            if guard.is_some() {
                AuthorityHostSignal::Ready
            } else if host.running.load(Ordering::Relaxed) {
                AuthorityHostSignal::Starting
            } else {
                AuthorityHostSignal::Closed
            }
        }
    }
}

pub(super) fn deliver_command_to_ready_host(
    host: &SessionSyncAuthorityHost,
    command: RemoteAuthorityCommand,
) -> Result<AuthorityHostSignal, LifecycleError> {
    let target_id = authority_command_target_id(&command).to_string();
    let mut guard = match host.writer.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            ERROR_LOG.log("[session-sync] authority writer mutex poisoned, recovering".to_string());
            poisoned.into_inner()
        }
    };

    while guard.is_none() && host.running.load(Ordering::Relaxed) {
        let wait_result = host
            .writer_ready
            .wait_timeout(guard, AUTHORITY_HOST_READY_TIMEOUT)
            .map_err(|_| {
                LifecycleError::Protocol(
                    "authority writer mutex poisoned while waiting for ready signal".to_string(),
                )
            })?;
        guard = wait_result.0;
        if wait_result.1.timed_out() {
            return Ok(AuthorityHostSignal::Starting);
        }
    }

    let Some(writer) = guard.as_mut() else {
        return Ok(AuthorityHostSignal::Closed);
    };
    let envelope = authority_command_envelope(command);
    if let Err(error) = write_control_plane_envelope(writer, &envelope) {
        let _ = writer.shutdown(Shutdown::Both);
        *guard = None;
        ERROR_LOG.log(format!(
            "[diag-timing] send_command_to_host: write failed for target={target_id}: {error}"
        ));
        return Ok(AuthorityHostSignal::Closed);
    }
    ERROR_LOG.log(format!(
        "[diag-timing] send_command_to_host: sent command to target={target_id}"
    ));
    Ok(AuthorityHostSignal::Ready)
}

fn authority_command_envelope(
    command: RemoteAuthorityCommand,
) -> ProtocolEnvelope<ControlPlanePayload> {
    let session_id = match &command {
        RemoteAuthorityCommand::OpenMirror(payload) => Some(payload.session_id.clone()),
        RemoteAuthorityCommand::CloseMirror(payload) => Some(payload.session_id.clone()),
        RemoteAuthorityCommand::RawPtyInput(payload) => Some(payload.session_id.clone()),
        RemoteAuthorityCommand::ApplyResize(payload) => Some(payload.session_id.clone()),
        RemoteAuthorityCommand::SyncRequest { .. } => None,
    };
    let payload = match command {
        RemoteAuthorityCommand::OpenMirror(payload) => {
            ControlPlanePayload::OpenMirrorRequest(payload)
        }
        RemoteAuthorityCommand::CloseMirror(payload) => {
            ControlPlanePayload::CloseMirrorRequest(payload)
        }
        RemoteAuthorityCommand::RawPtyInput(payload) => ControlPlanePayload::RawPtyInput(payload),
        RemoteAuthorityCommand::ApplyResize(payload) => ControlPlanePayload::ApplyResize(payload),
        RemoteAuthorityCommand::SyncRequest { .. } => ControlPlanePayload::Error(ErrorPayload {
            code: "local_sync_request_not_routable",
            message: "sync request is local to authority transport".to_string(),
            details: None,
        }),
    };
    ProtocolEnvelope {
        protocol_version: crate::infra::remote_protocol::REMOTE_PROTOCOL_VERSION.to_string(),
        message_id: format!("session-sync-authority-{}", timestamp_millis_now()),
        message_type: payload.message_type(),
        timestamp: format!("{}Z", timestamp_millis_now()),
        sender_id: SESSION_SYNC_AUTHORITY_ID.to_string(),
        correlation_id: None,
        session_id,
        target_id: None,
        attachment_id: None,
        console_id: None,
        payload,
    }
}

fn next_message_id_increment(next_message_id: &mut u64) {
    *next_message_id = next_message_id.saturating_add(1);
}

fn timestamp_millis_now() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

pub(crate) fn remote_session_published_envelope(
    node_id: &str,
    session_instance_id: &str,
    message_sequence: u64,
    session: &ManagedSessionRecord,
) -> GrpcNodeSessionEnvelope {
    let target_id = format!("remote-peer:{node_id}:{}", session.address.session_id());
    GrpcNodeSessionEnvelope {
        message_id: format!("{node_id}-session-sync-{message_sequence}"),
        sent_at: Some(timestamp_now()),
        session_instance_id: session_instance_id.to_string(),
        correlation_id: None,
        route: Some(RouteContext {
            authority_node_id: Some(node_id.to_string()),
            target_id: Some(target_id.clone()),
            attachment_id: None,
            console_id: None,
            console_host_id: None,
            session_id: Some(session.address.session_id().to_string()),
        }),
        body: Some(Body::TargetPublished(TargetPublished {
            target_id,
            authority_node_id: node_id.to_string(),
            transport: "tmux".to_string(),
            transport_session_id: session.address.session_id().to_string(),
            selector: session.selector.clone(),
            availability: session.availability.as_str().to_string(),
            command_name: session.command_name.clone(),
            current_path: session
                .current_path
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned()),
            attached_count: Some(session.attached_clients as u64),
            session_role: session.session_role.map(|role| role.as_str().to_string()),
            workspace_key: session.workspace_key.clone(),
            window_count: Some(session.window_count as u64),
            task_state: Some(session.task_state.as_str().to_string()),
            node_instance_id: session_instance_id.to_string(),
            revision: 0,
        })),
    }
}

pub(crate) fn remote_session_exited_envelope(
    node_id: &str,
    session_instance_id: &str,
    message_sequence: u64,
    transport_session_id: &str,
) -> GrpcNodeSessionEnvelope {
    let target_id = format!("remote-peer:{node_id}:{transport_session_id}");
    GrpcNodeSessionEnvelope {
        message_id: format!("{node_id}-session-sync-{message_sequence}"),
        sent_at: Some(timestamp_now()),
        session_instance_id: session_instance_id.to_string(),
        correlation_id: None,
        route: Some(RouteContext {
            authority_node_id: Some(node_id.to_string()),
            target_id: Some(target_id.clone()),
            attachment_id: None,
            console_id: None,
            console_host_id: None,
            session_id: Some(transport_session_id.to_string()),
        }),
        body: Some(Body::TargetExited(TargetExited {
            target_id,
            transport_session_id: transport_session_id.to_string(),
            node_instance_id: session_instance_id.to_string(),
            revision: 0,
        })),
    }
}

fn timestamp_now() -> prost_types::Timestamp {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    prost_types::Timestamp {
        seconds: now.as_secs() as i64,
        nanos: now.subsec_nanos() as i32,
    }
}

pub(super) fn remote_session_sync_owner_args(
    socket_name: &str,
    network: &RemoteNetworkConfig,
    ready_socket: Option<&Path>,
) -> Vec<String> {
    let mut args = vec![
        "__remote-session-sync-owner".to_string(),
        "--socket-name".to_string(),
        socket_name.to_string(),
    ];
    if let Some(ready_socket) = ready_socket {
        args.push("--ready-socket".to_string());
        args.push(ready_socket.display().to_string());
    }
    prepend_global_network_args(args, network)
}

pub(crate) fn remote_session_sync_owner_socket_path(socket_name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "waitagent-remote-session-sync-owner-{}.sock",
        sanitize_path_component(socket_name)
    ))
}

pub(crate) fn remote_session_sync_owner_available(socket_path: &Path) -> bool {
    send_owner_command(socket_path, "ping\n").is_ok()
}

pub(crate) fn notify_remote_session_sync_owner(
    socket_path: &Path,
    reason: LocalCatalogChangeReason,
) -> Result<(), LifecycleError> {
    send_owner_command(
        socket_path,
        &format!("local-catalog-changed {}\n", reason.encode()),
    )
}

pub(crate) fn shutdown_remote_session_sync_owner(socket_path: &Path) -> Result<(), LifecycleError> {
    send_owner_command(socket_path, "shutdown\n")
}

pub(super) fn serve_owner_commands(
    listener: UnixListener,
    local_catalog_tx: mpsc::Sender<LocalCatalogChangeReason>,
    shutdown_tx: mpsc::Sender<()>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else {
                break;
            };
            let mut request = String::new();
            let _ = stream.read_to_string(&mut request);
            let response = match parse_owner_command(request.trim()) {
                OwnerCommand::Ping => "ok\n".to_string(),
                OwnerCommand::LocalCatalogChanged(reason) => {
                    if local_catalog_tx.send(reason).is_ok() {
                        "ok\n".to_string()
                    } else {
                        "error local-catalog-channel-closed\n".to_string()
                    }
                }
                OwnerCommand::Shutdown => {
                    if shutdown_tx.send(()).is_ok() {
                        "ok\n".to_string()
                    } else {
                        "error shutdown-channel-closed\n".to_string()
                    }
                }
                OwnerCommand::Invalid(message) => format!("error {message}\n"),
            };
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    })
}

enum OwnerCommand {
    Ping,
    LocalCatalogChanged(LocalCatalogChangeReason),
    Shutdown,
    Invalid(String),
}

fn parse_owner_command(request: &str) -> OwnerCommand {
    if request.is_empty() || request == "ping" {
        return OwnerCommand::Ping;
    }
    if let Some(reason) = request.strip_prefix("local-catalog-changed ") {
        return LocalCatalogChangeReason::parse(reason)
            .map(OwnerCommand::LocalCatalogChanged)
            .unwrap_or_else(|| OwnerCommand::Invalid(format!("unknown-reason-{reason}")));
    }
    if request == "shutdown" {
        return OwnerCommand::Shutdown;
    }
    OwnerCommand::Invalid("unknown-command".to_string())
}

fn send_owner_command(socket_path: &Path, command: &str) -> Result<(), LifecycleError> {
    let mut stream = UnixStream::connect(socket_path).map_err(remote_session_sync_error)?;
    stream
        .write_all(command.as_bytes())
        .map_err(remote_session_sync_error)?;
    stream.shutdown(Shutdown::Write).ok();
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .map_err(remote_session_sync_error)?;
    if response.trim() == "ok" {
        Ok(())
    } else {
        Err(LifecycleError::Protocol(format!(
            "remote session sync owner rejected command `{}` with `{}`",
            command.trim(),
            response.trim()
        )))
    }
}

pub(super) fn backend_socket_still_exists(socket_name: &str) -> bool {
    let socket_path = crate::infra::tmux::tmux_socket_dir().join(socket_name);
    if !socket_path.exists() {
        return false;
    }
    EmbeddedTmuxBackend::from_build_env()
        .map(|backend| backend.socket_is_live(&TmuxSocketName::new(socket_name)))
        .unwrap_or(false)
}

fn sanitize_path_component(value: &str) -> String {
    value
        .chars()
        .map(|ch| match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' => ch,
            _ => '_',
        })
        .collect()
}
