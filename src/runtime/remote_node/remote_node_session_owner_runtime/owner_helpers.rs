use crate::cli::RemoteNetworkConfig;
use crate::domain::session_catalog::{
    ManagedSessionAddress, ManagedSessionRecord, ManagedSessionTaskState, SessionAvailability,
};
use crate::domain::workspace::{WorkspaceInstanceConfig, WorkspaceSessionRole};
use crate::infra::error_log::ERROR_LOG;
use crate::infra::remote_protocol::{
    ControlPlanePayload, CreateSessionRequestPayload, NodeSessionChannel, ProtocolEnvelope,
    REMOTE_PROTOCOL_VERSION,
};
use crate::infra::remote_transport_codec::{
    read_control_plane_envelope, write_control_plane_envelope,
};
use crate::infra::tmux::EmbeddedTmuxBackend;
use crate::lifecycle::LifecycleError;
use crate::runtime::remote_authority_target_host_runtime::remote_authority_target_host_args;
use crate::runtime::remote_authority_transport_runtime::RemoteAuthorityCommand;
use crate::runtime::remote_node_session_runtime::{
    RemoteNodeAuthorityEvent, RemoteNodeSessionError, RemoteNodeSessionRuntime,
};
use crate::runtime::remote_node_transport_runtime::{read_client_hello, write_server_hello};
use crate::runtime::remote_target_publication_runtime::{
    PublicationSenderCommand, RemoteTargetPublicationRuntime,
};
use crate::runtime::remote_target_publication_transport_runtime::RemoteTargetPublicationTransportRuntime;
use crate::runtime::sidecar_process_runtime::spawn_waitagent_sidecar;
use crate::runtime::target_host_runtime::TargetHostRuntime;
use std::collections::HashMap;
use std::fs;
use std::io;
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use std::hash::{Hash, Hasher};

const LIVE_AUTHORITY_SERVER_ID: &str = "waitagent-live-authority-owner";
const SHARED_AUTHORITY_RECONNECT_BASE_DELAY: Duration = Duration::from_millis(100);
const SHARED_AUTHORITY_RECONNECT_MAX_DELAY: Duration = Duration::from_secs(30);

pub(super) struct LiveSessionRoute {
    pub(super) socket_name: String,
    pub(super) target_session_name: String,
    pub(super) authority_id: String,
    pub(super) target_id: String,
    pub(super) transport_session_id: String,
    pub(super) socket_path: PathBuf,
    pub(super) running: Arc<AtomicBool>,
    pub(super) writer: Arc<Mutex<Option<UnixStream>>>,
    pub(super) pending_commands: Arc<Mutex<Vec<RemoteAuthorityCommand>>>,
}

#[derive(Clone)]
pub(super) struct PendingExit {
    target_session_name: String,
    transport_session_id: String,
}

#[derive(Clone)]
pub(super) struct SharedAuthoritySession {
    pub(super) authority_id: String,
    pub(super) transport_socket_path: String,
    pub(super) publication_runtime: RemoteTargetPublicationRuntime,
    pub(super) network: RemoteNetworkConfig,
    pub(super) running: Arc<AtomicBool>,
    pub(super) owner_started: Arc<AtomicBool>,
    pub(super) session: Arc<Mutex<Option<Arc<RemoteNodeSessionRuntime>>>>,
    pub(super) routes: Arc<Mutex<HashMap<String, Arc<LiveSessionRoute>>>>,
    pub(super) pending_exits: Arc<Mutex<HashMap<String, PendingExit>>>,
}

impl SharedAuthoritySession {
    fn current_session(&self) -> Option<Arc<RemoteNodeSessionRuntime>> {
        self.session
            .lock()
            .expect("shared authority session mutex should not be poisoned")
            .clone()
    }

    fn replace_session(&self, session: Option<Arc<RemoteNodeSessionRuntime>>) {
        *self
            .session
            .lock()
            .expect("shared authority session mutex should not be poisoned") = session;
    }

    fn clear_session_if_matches(&self, session: &Arc<RemoteNodeSessionRuntime>) {
        let mut guard = self
            .session
            .lock()
            .expect("shared authority session mutex should not be poisoned");
        if guard
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, session))
        {
            *guard = None;
        }
    }

    fn disconnect_session(&self, session: &Arc<RemoteNodeSessionRuntime>) {
        self.clear_session_if_matches(session);
        session.shutdown();
    }

    fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    fn should_stop_when_idle(&self) -> bool {
        let routes_empty = self
            .routes
            .lock()
            .expect("shared authority routes mutex should not be poisoned")
            .is_empty();
        let pending_exits_empty = self
            .pending_exits
            .lock()
            .expect("shared authority pending exits mutex should not be poisoned")
            .is_empty();
        routes_empty && pending_exits_empty
    }

    fn stop_if_idle(&self) -> bool {
        if !self.should_stop_when_idle() {
            return false;
        }
        self.running.store(false, Ordering::Relaxed);
        if let Some(session) = self
            .session
            .lock()
            .expect("shared authority session mutex should not be poisoned")
            .take()
        {
            session.shutdown();
        }
        true
    }

    fn has_pending_exits(&self) -> bool {
        !self
            .pending_exits
            .lock()
            .expect("shared authority pending exits mutex should not be poisoned")
            .is_empty()
    }

    fn queue_pending_exit(&self, route: &LiveSessionRoute) {
        self.pending_exits
            .lock()
            .expect("shared authority pending exits mutex should not be poisoned")
            .insert(
                route.target_session_name.clone(),
                PendingExit {
                    target_session_name: route.target_session_name.clone(),
                    transport_session_id: route.transport_session_id.clone(),
                },
            );
    }

    fn take_pending_exits(&self) -> Vec<PendingExit> {
        self.pending_exits
            .lock()
            .expect("shared authority pending exits mutex should not be poisoned")
            .drain()
            .map(|(_, pending)| pending)
            .collect()
    }

    fn restore_pending_exits(&self, pending_exits: impl IntoIterator<Item = PendingExit>) {
        let mut guard = self
            .pending_exits
            .lock()
            .expect("shared authority pending exits mutex should not be poisoned");
        for pending in pending_exits {
            guard.insert(pending.target_session_name.clone(), pending);
        }
    }

    fn dispatch_live_publication(&self, command: &PublicationSenderCommand) {
        let Some(session) = self.current_session() else {
            return;
        };
        if session.send_publication_sender_command(command).is_err() {
            self.disconnect_session(&session);
        }
    }
}

enum PublicationTransportDispatch<'a> {
    Publish {
        target: &'a ManagedSessionRecord,
        source_session_name: Option<&'a str>,
    },
    Exit {
        transport_session_id: &'a str,
        source_session_name: Option<&'a str>,
    },
}

pub(super) fn dispatch_publication_sender_command(
    publication_socket_path: &Path,
    transports: &mut HashMap<String, RemoteTargetPublicationTransportRuntime>,
    command: PublicationSenderCommand,
) -> Result<(), LifecycleError> {
    match command {
        PublicationSenderCommand::RegisterLiveSession { .. }
        | PublicationSenderCommand::UnregisterLiveSession { .. } => Ok(()),
        PublicationSenderCommand::PublishTarget {
            authority_id,
            transport_session_id,
            source_session_name,
            selector,
            availability,
            session_role,
            workspace_key,
            command_name,
            current_path,
            attached_clients,
            window_count,
            task_state,
        } => {
            let target = ManagedSessionRecord {
                address: ManagedSessionAddress::remote_peer(&authority_id, &transport_session_id),
                selector,
                availability: SessionAvailability::parse(availability).ok_or_else(|| {
                    LifecycleError::Protocol(format!(
                        "unsupported publication sender availability `{availability}`"
                    ))
                })?,
                workspace_dir: None,
                workspace_key,
                session_role: session_role
                    .map(|value| {
                        WorkspaceSessionRole::parse(value).ok_or_else(|| {
                            LifecycleError::Protocol(format!(
                                "unsupported publication sender session role `{value}`"
                            ))
                        })
                    })
                    .transpose()?,
                opened_by: Vec::new(),
                attached_clients,
                window_count,
                command_name,
                current_path: current_path.map(PathBuf::from),
                task_state: ManagedSessionTaskState::parse(task_state).ok_or_else(|| {
                    LifecycleError::Protocol(format!(
                        "unsupported publication sender task state `{task_state}`"
                    ))
                })?,
            };
            dispatch_cached_publication_transport_send(
                transports,
                publication_socket_path,
                &authority_id,
                PublicationTransportDispatch::Publish {
                    target: &target,
                    source_session_name: source_session_name.as_deref(),
                },
            )
        }
        PublicationSenderCommand::ExitTarget {
            authority_id,
            transport_session_id,
            source_session_name,
        } => dispatch_cached_publication_transport_send(
            transports,
            publication_socket_path,
            &authority_id,
            PublicationTransportDispatch::Exit {
                transport_session_id: &transport_session_id,
                source_session_name: source_session_name.as_deref(),
            },
        ),
    }
}

fn dispatch_cached_publication_transport_send(
    transports: &mut HashMap<String, RemoteTargetPublicationTransportRuntime>,
    publication_socket_path: &Path,
    authority_id: &str,
    dispatch: PublicationTransportDispatch<'_>,
) -> Result<(), LifecycleError> {
    let send_once = |transport: &RemoteTargetPublicationTransportRuntime| match &dispatch {
        PublicationTransportDispatch::Publish {
            target,
            source_session_name,
        } => transport
            .send_target_published(target, *source_session_name)
            .map_err(publication_owner_error),
        PublicationTransportDispatch::Exit {
            transport_session_id,
            source_session_name,
        } => transport
            .send_target_exited(transport_session_id, *source_session_name)
            .map_err(publication_owner_error),
    };

    if !transports.contains_key(authority_id) {
        let transport =
            RemoteTargetPublicationTransportRuntime::connect(publication_socket_path, authority_id)
                .map_err(publication_owner_error)?;
        transports.insert(authority_id.to_string(), transport);
    }

    match send_once(transports.get(authority_id).ok_or_else(|| {
        LifecycleError::Protocol("publication transport cache missing entry".to_string())
    })?) {
        Ok(()) => Ok(()),
        Err(_) => {
            transports.remove(authority_id);
            let transport = RemoteTargetPublicationTransportRuntime::connect(
                publication_socket_path,
                authority_id,
            )
            .map_err(publication_owner_error)?;
            let result = send_once(&transport);
            transports.insert(authority_id.to_string(), transport);
            result
        }
    }
}

fn publication_owner_error<E>(error: E) -> LifecycleError
where
    E: ToString,
{
    LifecycleError::Io(
        "remote node session owner failure".to_string(),
        io::Error::new(io::ErrorKind::Other, error.to_string()),
    )
}

pub(crate) fn live_authority_session_socket_path(
    socket_name: &str,
    target_session_name: &str,
) -> PathBuf {
    let hash = stable_socket_hash(&[socket_name, target_session_name]);
    std::env::temp_dir().join(format!("waitagent-live-authority-{hash}.sock"))
}

#[cfg(test)]
pub(crate) fn spawn_live_authority_session_bridge(
    socket_path: PathBuf,
    session: Arc<RemoteNodeSessionRuntime>,
    running: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let Ok(listener) = bind_live_authority_listener(&socket_path) else {
            return;
        };
        while running.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((stream, _)) => {
                    let _ = stream.set_nonblocking(false);
                    let _ = bridge_live_authority_stream(stream, session.clone(), running.clone());
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }
        let _ = fs::remove_file(&socket_path);
    })
}

pub(super) fn ensure_live_session_route(
    current_executable: &Path,
    socket_name: &str,
    target_session_name: &str,
    authority_id: &str,
    target_id: &str,
    transport_socket_path: &str,
    network: &RemoteNetworkConfig,
    publication_runtime: &RemoteTargetPublicationRuntime,
    live_sessions: &mut HashMap<String, Arc<LiveSessionRoute>>,
    authority_sessions: &mut HashMap<String, SharedAuthoritySession>,
) -> Result<(), LifecycleError> {
    ensure_live_session_route_with_target_host_mode(
        current_executable,
        socket_name,
        target_session_name,
        authority_id,
        target_id,
        transport_socket_path,
        network,
        publication_runtime,
        live_sessions,
        authority_sessions,
        true,
        true,
    )
}

#[cfg(test)]
pub(super) fn ensure_live_session_route_without_target_host_sidecar(
    current_executable: &Path,
    socket_name: &str,
    target_session_name: &str,
    authority_id: &str,
    target_id: &str,
    transport_socket_path: &str,
    network: &RemoteNetworkConfig,
    publication_runtime: &RemoteTargetPublicationRuntime,
    live_sessions: &mut HashMap<String, Arc<LiveSessionRoute>>,
    authority_sessions: &mut HashMap<String, SharedAuthoritySession>,
) -> Result<(), LifecycleError> {
    ensure_live_session_route_with_target_host_mode(
        current_executable,
        socket_name,
        target_session_name,
        authority_id,
        target_id,
        transport_socket_path,
        network,
        publication_runtime,
        live_sessions,
        authority_sessions,
        false,
        true,
    )
}

#[cfg(test)]
pub(super) fn ensure_live_session_route_without_target_host_or_dispatcher(
    current_executable: &Path,
    socket_name: &str,
    target_session_name: &str,
    authority_id: &str,
    target_id: &str,
    transport_socket_path: &str,
    network: &RemoteNetworkConfig,
    publication_runtime: &RemoteTargetPublicationRuntime,
    live_sessions: &mut HashMap<String, Arc<LiveSessionRoute>>,
    authority_sessions: &mut HashMap<String, SharedAuthoritySession>,
) -> Result<(), LifecycleError> {
    ensure_live_session_route_with_target_host_mode(
        current_executable,
        socket_name,
        target_session_name,
        authority_id,
        target_id,
        transport_socket_path,
        network,
        publication_runtime,
        live_sessions,
        authority_sessions,
        false,
        false,
    )
}

fn ensure_live_session_route_with_target_host_mode(
    current_executable: &Path,
    socket_name: &str,
    target_session_name: &str,
    authority_id: &str,
    target_id: &str,
    transport_socket_path: &str,
    network: &RemoteNetworkConfig,
    publication_runtime: &RemoteTargetPublicationRuntime,
    live_sessions: &mut HashMap<String, Arc<LiveSessionRoute>>,
    authority_sessions: &mut HashMap<String, SharedAuthoritySession>,
    start_target_host_sidecar: bool,
    start_shared_dispatcher: bool,
) -> Result<(), LifecycleError> {
    if let Some(existing) = live_sessions.get(target_session_name) {
        let transport_session_id = target_id
            .strip_prefix(format!("remote-peer:{authority_id}:").as_str())
            .ok_or_else(|| {
                LifecycleError::Protocol(format!(
                    "live target id `{target_id}` does not match authority `{authority_id}`"
                ))
            })?;
        if existing.authority_id == authority_id
            && existing.target_id == target_id
            && existing.transport_session_id == transport_session_id
            && existing.socket_name == socket_name
        {
            ensure_shared_authority_session(
                authority_id,
                transport_socket_path,
                network,
                publication_runtime.clone(),
                authority_sessions,
            )?;
            let shared_session = authority_sessions
                .get(authority_id)
                .expect("authority session should exist after ensure");
            if start_shared_dispatcher {
                start_shared_authority_command_dispatcher(shared_session.clone());
            }
            return Ok(());
        }
    }
    stop_live_session_route(target_session_name, live_sessions, authority_sessions);
    ensure_shared_authority_session(
        authority_id,
        transport_socket_path,
        network,
        publication_runtime.clone(),
        authority_sessions,
    )?;
    let shared_session = authority_sessions
        .get(authority_id)
        .expect("authority session should exist after ensure");
    let transport_session_id = target_id
        .strip_prefix(format!("remote-peer:{authority_id}:").as_str())
        .ok_or_else(|| {
            LifecycleError::Protocol(format!(
                "live target id `{target_id}` does not match authority `{authority_id}`"
            ))
        })?
        .to_string();
    let route = Arc::new(LiveSessionRoute {
        socket_name: socket_name.to_string(),
        target_session_name: target_session_name.to_string(),
        authority_id: authority_id.to_string(),
        target_id: target_id.to_string(),
        transport_session_id,
        socket_path: live_authority_session_socket_path(socket_name, target_session_name),
        running: Arc::new(AtomicBool::new(true)),
        writer: Arc::new(Mutex::new(None)),
        pending_commands: Arc::new(Mutex::new(Vec::new())),
    });
    spawn_live_authority_route_listener(shared_session.clone(), route.clone());
    if start_target_host_sidecar {
        spawn_remote_authority_target_host(
            current_executable,
            &route,
            transport_socket_path,
            network,
        )?;
    }
    shared_session
        .routes
        .lock()
        .expect("shared authority routes mutex should not be poisoned")
        .insert(target_session_name.to_string(), route.clone());
    if start_shared_dispatcher {
        start_shared_authority_command_dispatcher(shared_session.clone());
    }
    live_sessions.insert(target_session_name.to_string(), route);
    Ok(())
}

pub(super) fn stop_live_session_route(
    target_session_name: &str,
    live_sessions: &mut HashMap<String, Arc<LiveSessionRoute>>,
    authority_sessions: &mut HashMap<String, SharedAuthoritySession>,
) {
    if let Some(route) = live_sessions.remove(target_session_name) {
        route.running.store(false, Ordering::Relaxed);
        if let Some(writer) = route
            .writer
            .lock()
            .expect("live session writer mutex should not be poisoned")
            .take()
        {
            let _ = writer.shutdown(Shutdown::Both);
        }
        let _ = fs::remove_file(&route.socket_path);
        let mut remove_authority = false;
        if let Some(shared_session) = authority_sessions.get(&route.authority_id) {
            if shared_session.current_session().is_none()
                && shared_session.owner_started.load(Ordering::Relaxed)
            {
                shared_session.queue_pending_exit(&route);
            }
            let mut routes = shared_session
                .routes
                .lock()
                .expect("shared authority routes mutex should not be poisoned");
            routes.remove(target_session_name);
            drop(routes);
            remove_authority = shared_session.stop_if_idle();
        }
        if remove_authority {
            authority_sessions.remove(&route.authority_id);
        }
    }
}

pub(super) fn dispatch_live_publication(
    target_session_name: &str,
    command: &PublicationSenderCommand,
    live_sessions: &mut HashMap<String, Arc<LiveSessionRoute>>,
    authority_sessions: &mut HashMap<String, SharedAuthoritySession>,
) -> bool {
    let Some(route) = live_sessions.get(target_session_name) else {
        return false;
    };
    if let Some(shared_session) = authority_sessions.get(&route.authority_id) {
        shared_session.dispatch_live_publication(command);
        return true;
    }
    false
}

fn ensure_shared_authority_session(
    authority_id: &str,
    transport_socket_path: &str,
    network: &RemoteNetworkConfig,
    publication_runtime: RemoteTargetPublicationRuntime,
    authority_sessions: &mut HashMap<String, SharedAuthoritySession>,
) -> Result<(), LifecycleError> {
    reap_inactive_authority_sessions(authority_sessions);
    if authority_sessions.contains_key(authority_id) {
        return Ok(());
    }
    let shared_session = SharedAuthoritySession {
        authority_id: authority_id.to_string(),
        transport_socket_path: transport_socket_path.to_string(),
        publication_runtime,
        network: network.clone(),
        running: Arc::new(AtomicBool::new(true)),
        owner_started: Arc::new(AtomicBool::new(false)),
        session: Arc::new(Mutex::new(None)),
        routes: Arc::new(Mutex::new(HashMap::<String, Arc<LiveSessionRoute>>::new())),
        pending_exits: Arc::new(Mutex::new(HashMap::new())),
    };
    authority_sessions.insert(authority_id.to_string(), shared_session);
    Ok(())
}

fn start_shared_authority_command_dispatcher(shared_session: SharedAuthoritySession) {
    if shared_session
        .owner_started
        .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
        .is_err()
    {
        return;
    }
    thread::spawn(move || {
        let mut reconnect_attempt = 0_u32;
        while shared_session.is_running() {
            if shared_session.stop_if_idle() {
                break;
            }
            let session = match RemoteNodeSessionRuntime::connect(
                &shared_session.transport_socket_path,
                &shared_session.authority_id,
                shared_session.network.connect_endpoint_uri().as_deref(),
            ) {
                Ok(session) => {
                    if reconnect_attempt > 0 {
                        eprintln!(
                            "[waitagent-diag] authority session reconnected after {} attempt(s) for {}",
                            reconnect_attempt, shared_session.authority_id,
                        );
                    }
                    reconnect_attempt = 0;
                    Arc::new(session)
                }
                Err(error) => {
                    if reconnect_attempt == 0 || reconnect_attempt % 5 == 0 {
                        eprintln!(
                            "[waitagent-diag] authority session connect failed (attempt={}) for {}: {}",
                            reconnect_attempt, shared_session.authority_id, error,
                        );
                    }
                    thread::sleep(shared_authority_reconnect_delay(reconnect_attempt));
                    reconnect_attempt = reconnect_attempt.saturating_add(1);
                    continue;
                }
            };
            shared_session.replace_session(Some(session.clone()));
            if (reconnect_attempt > 0 || shared_session.has_pending_exits())
                && !restore_shared_authority_state(&shared_session, &session)
            {
                eprintln!(
                    "[waitagent-diag] authority state restore failed, retrying (attempt={}) for {}",
                    reconnect_attempt, shared_session.authority_id,
                );
                reconnect_attempt = reconnect_attempt.saturating_add(1);
                thread::sleep(shared_authority_reconnect_delay(reconnect_attempt));
                continue;
            }
            while shared_session.is_running() {
                if shared_session.stop_if_idle() {
                    break;
                }
                match session.recv_authority_event() {
                    Ok(RemoteNodeAuthorityEvent::Command(command)) => {
                        let _ = dispatch_authority_command_to_live_route(
                            &shared_session.routes,
                            &command,
                        );
                    }
                    Ok(RemoteNodeAuthorityEvent::CreateSessionRequest {
                        payload,
                        correlation_id,
                    }) => {
                        if let Err(error) = handle_create_session_request(
                            &shared_session,
                            &session,
                            payload,
                            correlation_id.as_deref(),
                        ) {
                            eprintln!(
                                "[waitagent-diag] create-session handler failed for {}: {}",
                                shared_session.authority_id, error
                            );
                        }
                    }
                    Ok(RemoteNodeAuthorityEvent::CreateSessionAccepted(_))
                    | Ok(RemoteNodeAuthorityEvent::CreateSessionRejected(_)) => {
                        // Replies are consumed by the server-side creation service in a later slice.
                    }
                    Err(error) => {
                        eprintln!(
                            "[waitagent-diag] authority command stream error for {}: {}",
                            shared_session.authority_id, error,
                        );
                        shared_session.disconnect_session(&session);
                        mark_live_routes_offline(
                            &shared_session.publication_runtime,
                            &shared_session.routes,
                        );
                        reconnect_attempt = reconnect_attempt.saturating_add(1);
                        thread::sleep(shared_authority_reconnect_delay(reconnect_attempt));
                        break;
                    }
                }
            }
        }
        if reconnect_attempt > 0 {
            eprintln!(
                "[waitagent-diag] authority session dispatcher exiting after {} reconnect attempts for {}",
                reconnect_attempt, shared_session.authority_id,
            );
        }
        shutdown_live_routes(&shared_session.routes);
        shared_session.replace_session(None);
    });
}

fn restore_shared_authority_state(
    shared_session: &SharedAuthoritySession,
    session: &Arc<RemoteNodeSessionRuntime>,
) -> bool {
    let pending_exits = shared_session.take_pending_exits();
    for (index, pending_exit) in pending_exits.iter().enumerate() {
        if session
            .send_target_exited(
                &pending_exit.transport_session_id,
                Some(&pending_exit.target_session_name),
            )
            .is_err()
        {
            shared_session.disconnect_session(session);
            shared_session.restore_pending_exits(pending_exits.into_iter().skip(index));
            mark_live_routes_offline(&shared_session.publication_runtime, &shared_session.routes);
            return false;
        }
    }
    let live_routes = shared_session
        .routes
        .lock()
        .expect("shared authority routes mutex should not be poisoned")
        .values()
        .cloned()
        .collect::<Vec<_>>();
    for route in live_routes {
        let refreshed = shared_session
            .publication_runtime
            .signal_cached_source_session_refresh(&route.socket_name, &route.target_session_name)
            .unwrap_or(false);
        if !refreshed {
            let _ = shared_session
                .publication_runtime
                .signal_source_session_refresh(&route.socket_name, &route.target_session_name);
        }
    }
    true
}

fn handle_create_session_request(
    shared_session: &SharedAuthoritySession,
    session: &Arc<RemoteNodeSessionRuntime>,
    payload: CreateSessionRequestPayload,
    correlation_id: Option<&str>,
) -> Result<(), LifecycleError> {
    let result = create_local_target_for_request(shared_session, &payload);
    match result {
        Ok(created) => session
            .send_create_session_accepted(
                &payload.request_id,
                &created.session_id,
                &created.target_id,
                correlation_id,
            )
            .map_err(publication_owner_error),
        Err(error) => {
            let message = error.to_string();
            session
                .send_create_session_rejected(
                    &payload.request_id,
                    "create_session_failed",
                    message,
                    correlation_id,
                )
                .map_err(publication_owner_error)
        }
    }
}

struct CreatedRemoteTargetSession {
    session_id: String,
    target_id: String,
}

fn create_local_target_for_request(
    shared_session: &SharedAuthoritySession,
    payload: &CreateSessionRequestPayload,
) -> Result<CreatedRemoteTargetSession, LifecycleError> {
    if payload.authority_node_id != shared_session.authority_id {
        return Err(LifecycleError::Protocol(format!(
            "create-session request for authority `{}` reached `{}`",
            payload.authority_node_id, shared_session.authority_id
        )));
    }
    let cwd = select_create_session_cwd(payload);
    let runtime = TargetHostRuntime::from_build_env_with_network_and_executable(
        EmbeddedTmuxBackend::from_build_env().map_err(publication_owner_error)?,
        shared_session.network.clone(),
        crate::runtime::current_executable::current_waitagent_executable()?,
    )?;
    let workspace = runtime
        .ensure_target_host(WorkspaceInstanceConfig::for_new_target_on_socket_with_size(
            &cwd,
            shared_session.authority_id.clone(),
            u16::try_from(payload.rows).ok().filter(|rows| *rows > 0),
            u16::try_from(payload.cols).ok().filter(|cols| *cols > 0),
        ))
        .map_err(publication_owner_error)?;
    let session_id = workspace.workspace_handle.session_name.as_str().to_string();
    Ok(CreatedRemoteTargetSession {
        target_id: format!("remote-peer:{}:{}", shared_session.authority_id, session_id),
        session_id,
    })
}

fn select_create_session_cwd(payload: &CreateSessionRequestPayload) -> PathBuf {
    payload
        .cwd_hint
        .as_deref()
        .map(PathBuf::from)
        .filter(|path| path.is_dir())
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}

fn shared_authority_reconnect_delay(attempt: u32) -> Duration {
    // Full jitter: delay = random_between(base, min(max_delay, base * 2^attempt))
    // Uses a hash of the attempt number for deterministic pseudo-random jitter,
    // which avoids an external rand dependency while still spreading out
    // reconnection attempts across peers.
    let multiplier = 1_u32 << attempt.min(30);
    let max = std::cmp::min(
        SHARED_AUTHORITY_RECONNECT_BASE_DELAY.saturating_mul(multiplier),
        SHARED_AUTHORITY_RECONNECT_MAX_DELAY,
    );
    let base_ms = SHARED_AUTHORITY_RECONNECT_BASE_DELAY.as_millis() as u64;
    let max_ms = max.as_millis() as u64;
    if max_ms <= base_ms {
        return max;
    }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    attempt.hash(&mut hasher);
    let range = max_ms - base_ms + 1;
    let jitter = hasher.finish() % range;
    Duration::from_millis(base_ms + jitter)
}

pub(super) fn reap_inactive_authority_sessions(
    authority_sessions: &mut HashMap<String, SharedAuthoritySession>,
) {
    let inactive = authority_sessions
        .iter()
        .filter(|(_, session)| !session.is_running())
        .map(|(authority_id, _)| authority_id.clone())
        .collect::<Vec<_>>();
    for authority_id in inactive {
        authority_sessions.remove(&authority_id);
    }
}

pub(super) fn dispatch_authority_command_to_live_route(
    routes: &Arc<Mutex<HashMap<String, Arc<LiveSessionRoute>>>>,
    command: &RemoteAuthorityCommand,
) -> Result<(), RemoteNodeSessionError> {
    let target_id = authority_command_target_id(command);
    let route = routes
        .lock()
        .expect("shared authority routes mutex should not be poisoned")
        .values()
        .find(|route| route.target_id == target_id)
        .cloned();
    let Some(route) = route else {
        return Ok(());
    };
    let mut writer_guard = route
        .writer
        .lock()
        .expect("live session writer mutex should not be poisoned");
    let Some(writer) = writer_guard.as_mut() else {
        route
            .pending_commands
            .lock()
            .expect("live session pending commands mutex should not be poisoned")
            .push(command.clone());
        return Ok(());
    };
    if let Err(error) =
        write_control_plane_envelope(writer, &authority_command_envelope(command.clone()))
    {
        let _ = writer.shutdown(Shutdown::Both);
        *writer_guard = None;
        route
            .pending_commands
            .lock()
            .expect("live session pending commands mutex should not be poisoned")
            .push(command.clone());
        return Err(RemoteNodeSessionError::new(error.to_string()));
    }
    Ok(())
}

fn spawn_remote_authority_target_host(
    current_executable: &Path,
    route: &Arc<LiveSessionRoute>,
    transport_socket_path: &str,
    network: &RemoteNetworkConfig,
) -> Result<(), LifecycleError> {
    spawn_waitagent_sidecar(
        current_executable,
        remote_authority_target_host_args(
            &route.socket_name,
            &route.target_session_name,
            &route.transport_session_id,
            &route.authority_id,
            &route.target_id,
            transport_socket_path,
            network,
        ),
    )
    .map_err(|error| {
        LifecycleError::Io(
            "failed to start remote authority target host".to_string(),
            error,
        )
    })
}

fn authority_command_target_id(command: &RemoteAuthorityCommand) -> &str {
    match command {
        RemoteAuthorityCommand::OpenMirror(payload) => payload.target_id.as_str(),
        RemoteAuthorityCommand::CloseMirror(payload) => payload.target_id.as_str(),
        RemoteAuthorityCommand::RawPtyInput(payload) => payload.target_id.as_str(),
        RemoteAuthorityCommand::ApplyResize(payload) => payload.target_id.as_str(),
    }
}

fn shutdown_live_routes(routes: &Arc<Mutex<HashMap<String, Arc<LiveSessionRoute>>>>) {
    let live_routes = routes
        .lock()
        .expect("shared authority routes mutex should not be poisoned")
        .values()
        .cloned()
        .collect::<Vec<_>>();
    for route in live_routes {
        route.running.store(false, Ordering::Relaxed);
        if let Some(writer) = route
            .writer
            .lock()
            .expect("live session writer mutex should not be poisoned")
            .take()
        {
            let _ = writer.shutdown(Shutdown::Both);
        }
        let _ = fs::remove_file(&route.socket_path);
    }
}

fn mark_live_routes_offline(
    publication_runtime: &RemoteTargetPublicationRuntime,
    routes: &Arc<Mutex<HashMap<String, Arc<LiveSessionRoute>>>>,
) {
    let live_routes = routes
        .lock()
        .expect("shared authority routes mutex should not be poisoned")
        .values()
        .cloned()
        .collect::<Vec<_>>();
    for route in live_routes {
        let _ = publication_runtime.mark_source_target_offline(
            &route.socket_name,
            &route.target_session_name,
            &route.target_id,
        );
    }
}

fn spawn_live_authority_route_listener(
    shared_session: SharedAuthoritySession,
    route: Arc<LiveSessionRoute>,
) {
    let socket_path = route.socket_path.clone();
    thread::spawn(move || {
        let Ok(listener) = bind_live_authority_listener(&socket_path) else {
            return;
        };
        while route.running.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((stream, _)) => {
                    let _ = bridge_shared_live_authority_stream(
                        stream,
                        shared_session.clone(),
                        route.clone(),
                    );
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }
        let _ = fs::remove_file(&socket_path);
    });
}

pub(super) fn bridge_shared_live_authority_stream(
    mut host_stream: UnixStream,
    shared_session: SharedAuthoritySession,
    route: Arc<LiveSessionRoute>,
) -> Result<(), RemoteNodeSessionError> {
    let _node_id = read_client_hello(&mut host_stream)?;
    write_server_hello(&mut host_stream, LIVE_AUTHORITY_SERVER_ID)?;
    let host_reader = host_stream.try_clone()?;
    {
        let mut writer_guard = route
            .writer
            .lock()
            .expect("live session writer mutex should not be poisoned");
        if let Some(previous) = writer_guard.take() {
            let _ = previous.shutdown(Shutdown::Both);
        }
        let writer_clone = host_stream.try_clone()?;
        // 5s write timeout prevents indefinite blocking when the target-host
        // sub-process dies (e.g. after a WiFi blip tears down the bridge) but
        // the route is not yet cleaned up on the owner side.
        let _ = writer_clone.set_write_timeout(Some(Duration::from_secs(5)));
        *writer_guard = Some(writer_clone);
    }
    flush_pending_live_route_commands(&route)?;
    let forward_result =
        forward_host_output_to_shared_session(host_reader, shared_session, route.running.clone());
    let _ = host_stream.shutdown(Shutdown::Both);
    let _ = route
        .writer
        .lock()
        .expect("live session writer mutex should not be poisoned")
        .take();
    forward_result
}

fn bind_live_authority_listener(socket_path: &Path) -> Result<UnixListener, io::Error> {
    if socket_path.exists() {
        let _ = fs::remove_file(socket_path);
    }
    let listener = UnixListener::bind(socket_path)?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

#[cfg(test)]
fn bridge_live_authority_stream(
    mut host_stream: UnixStream,
    session: Arc<RemoteNodeSessionRuntime>,
    running: Arc<AtomicBool>,
) -> Result<(), RemoteNodeSessionError> {
    let _node_id = read_client_hello(&mut host_stream)?;
    write_server_hello(&mut host_stream, LIVE_AUTHORITY_SERVER_ID)?;
    let host_reader = host_stream.try_clone()?;
    let host_session = session.clone();
    let host_running = running.clone();
    let forward_host = thread::spawn(move || {
        let _ = forward_host_output_to_session(host_reader, host_session, host_running);
    });
    while running.load(Ordering::Relaxed) {
        let command = match session.recv_authority_event() {
            Ok(RemoteNodeAuthorityEvent::Command(command)) => command,
            Ok(other) => {
                running.store(false, Ordering::Relaxed);
                let _ = host_stream.shutdown(Shutdown::Write);
                let _ = forward_host.join();
                let _ = host_stream.shutdown(Shutdown::Both);
                return Err(RemoteNodeSessionError::new(format!(
                    "unexpected authority event in live bridge: {other:?}"
                )));
            }
            Err(error) => {
                running.store(false, Ordering::Relaxed);
                let _ = host_stream.shutdown(Shutdown::Write);
                let _ = forward_host.join();
                let _ = host_stream.shutdown(Shutdown::Both);
                return Err(error);
            }
        };
        write_control_plane_envelope(&mut host_stream, &authority_command_envelope(command))?;
    }
    let _ = host_stream.shutdown(Shutdown::Write);
    let _ = forward_host.join();
    let _ = host_stream.shutdown(Shutdown::Both);
    Ok(())
}

#[cfg(test)]
fn forward_host_output_to_session(
    mut host_reader: UnixStream,
    session: Arc<RemoteNodeSessionRuntime>,
    running: Arc<AtomicBool>,
) -> Result<(), RemoteNodeSessionError> {
    let _ = running;
    loop {
        let envelope = read_control_plane_envelope(&mut host_reader)?;
        match envelope.payload {
            ControlPlanePayload::OpenMirrorAccepted(payload) => {
                let session_id = payload.session_id.clone();
                let target_id = payload.target_id.clone();
                session.send_payload(
                    NodeSessionChannel::Authority,
                    &session_id,
                    &target_id,
                    "authority-msg",
                    ControlPlanePayload::OpenMirrorAccepted(payload),
                )?;
            }
            ControlPlanePayload::OpenMirrorRejected(payload) => {
                let session_id = payload.session_id.clone();
                let target_id = payload.target_id.clone();
                session.send_payload(
                    NodeSessionChannel::Authority,
                    &session_id,
                    &target_id,
                    "authority-msg",
                    ControlPlanePayload::OpenMirrorRejected(payload),
                )?;
            }
            ControlPlanePayload::MirrorBootstrapChunk(payload) => {
                let session_id = payload.session_id.clone();
                let target_id = payload.target_id.clone();
                session.send_payload(
                    NodeSessionChannel::Authority,
                    &session_id,
                    &target_id,
                    "authority-msg",
                    ControlPlanePayload::MirrorBootstrapChunk(payload),
                )?;
            }
            ControlPlanePayload::MirrorBootstrapComplete(payload) => {
                let session_id = payload.session_id.clone();
                let target_id = payload.target_id.clone();
                session.send_payload(
                    NodeSessionChannel::Authority,
                    &session_id,
                    &target_id,
                    "authority-msg",
                    ControlPlanePayload::MirrorBootstrapComplete(payload),
                )?;
            }
            ControlPlanePayload::TargetOutput(payload) => {
                session.send_target_output(
                    &payload.session_id,
                    &payload.target_id,
                    payload.output_seq,
                    payload.stream,
                    payload.output_bytes,
                )?;
            }
            ControlPlanePayload::TargetExited(payload) => {
                session.send_target_exited(
                    &payload.transport_session_id,
                    payload.source_session_name.as_deref(),
                )?;
                return Ok(());
            }
            other => {
                return Err(RemoteNodeSessionError::new(format!(
                    "unexpected live authority host payload `{}`",
                    other.message_type()
                )));
            }
        }
    }
}

fn flush_pending_live_route_commands(
    route: &Arc<LiveSessionRoute>,
) -> Result<(), RemoteNodeSessionError> {
    let pending = {
        let mut guard = route
            .pending_commands
            .lock()
            .expect("live session pending commands mutex should not be poisoned");
        std::mem::take(&mut *guard)
    };
    if pending.is_empty() {
        return Ok(());
    }
    let mut writer_guard = route
        .writer
        .lock()
        .expect("live session writer mutex should not be poisoned");
    let Some(writer) = writer_guard.as_mut() else {
        let mut guard = route
            .pending_commands
            .lock()
            .expect("live session pending commands mutex should not be poisoned");
        guard.extend(pending);
        return Ok(());
    };
    for command in pending {
        if let Err(error) =
            write_control_plane_envelope(writer, &authority_command_envelope(command.clone()))
        {
            let _ = writer.shutdown(Shutdown::Both);
            *writer_guard = None;
            let mut guard = route
                .pending_commands
                .lock()
                .expect("live session pending commands mutex should not be poisoned");
            guard.push(command);
            return Err(RemoteNodeSessionError::new(error.to_string()));
        }
    }
    Ok(())
}

fn forward_host_output_to_shared_session(
    mut host_reader: UnixStream,
    shared_session: SharedAuthoritySession,
    running: Arc<AtomicBool>,
) -> Result<(), RemoteNodeSessionError> {
    while running.load(Ordering::Relaxed) {
        let envelope = read_control_plane_envelope(&mut host_reader)?;
        match envelope.payload {
            ControlPlanePayload::OpenMirrorAccepted(payload) => {
                let Some(session) = shared_session.current_session() else {
                    continue;
                };
                let session_id = payload.session_id.clone();
                let target_id = payload.target_id.clone();
                if session
                    .send_payload(
                        NodeSessionChannel::Authority,
                        &session_id,
                        &target_id,
                        "authority-msg",
                        ControlPlanePayload::OpenMirrorAccepted(payload),
                    )
                    .is_err()
                {
                    shared_session.disconnect_session(&session);
                    mark_live_routes_offline(
                        &shared_session.publication_runtime,
                        &shared_session.routes,
                    );
                }
            }
            ControlPlanePayload::OpenMirrorRejected(payload) => {
                let Some(session) = shared_session.current_session() else {
                    continue;
                };
                let session_id = payload.session_id.clone();
                let target_id = payload.target_id.clone();
                if session
                    .send_payload(
                        NodeSessionChannel::Authority,
                        &session_id,
                        &target_id,
                        "authority-msg",
                        ControlPlanePayload::OpenMirrorRejected(payload),
                    )
                    .is_err()
                {
                    shared_session.disconnect_session(&session);
                    mark_live_routes_offline(
                        &shared_session.publication_runtime,
                        &shared_session.routes,
                    );
                }
            }
            ControlPlanePayload::MirrorBootstrapChunk(payload) => {
                let Some(session) = shared_session.current_session() else {
                    continue;
                };
                let session_id = payload.session_id.clone();
                let target_id = payload.target_id.clone();
                if session
                    .send_payload(
                        NodeSessionChannel::Authority,
                        &session_id,
                        &target_id,
                        "authority-msg",
                        ControlPlanePayload::MirrorBootstrapChunk(payload),
                    )
                    .is_err()
                {
                    shared_session.disconnect_session(&session);
                    mark_live_routes_offline(
                        &shared_session.publication_runtime,
                        &shared_session.routes,
                    );
                }
            }
            ControlPlanePayload::MirrorBootstrapComplete(payload) => {
                let Some(session) = shared_session.current_session() else {
                    continue;
                };
                let session_id = payload.session_id.clone();
                let target_id = payload.target_id.clone();
                if session
                    .send_payload(
                        NodeSessionChannel::Authority,
                        &session_id,
                        &target_id,
                        "authority-msg",
                        ControlPlanePayload::MirrorBootstrapComplete(payload),
                    )
                    .is_err()
                {
                    shared_session.disconnect_session(&session);
                    mark_live_routes_offline(
                        &shared_session.publication_runtime,
                        &shared_session.routes,
                    );
                }
            }
            ControlPlanePayload::TargetOutput(payload) => {
                let Some(session) = shared_session.current_session() else {
                    continue;
                };
                if session
                    .send_target_output(
                        &payload.session_id,
                        &payload.target_id,
                        payload.output_seq,
                        payload.stream,
                        payload.output_bytes,
                    )
                    .is_err()
                {
                    shared_session.disconnect_session(&session);
                    mark_live_routes_offline(
                        &shared_session.publication_runtime,
                        &shared_session.routes,
                    );
                }
            }
            ControlPlanePayload::RawPtyOutput(payload) => {
                let Some(session) = shared_session.current_session() else {
                    continue;
                };
                if session
                    .send_raw_pty_output(
                        &payload.session_id,
                        &payload.target_id,
                        payload.output_seq,
                        payload.output_bytes,
                    )
                    .is_err()
                {
                    shared_session.disconnect_session(&session);
                    mark_live_routes_offline(
                        &shared_session.publication_runtime,
                        &shared_session.routes,
                    );
                }
            }
            ControlPlanePayload::TargetExited(payload) => {
                ERROR_LOG.log(format!(
                    "[diag-bug] forward_host_output_to_shared_session: received TargetExited, transport_session_id={}, source_session_name={:?}, has_session={}",
                    payload.transport_session_id,
                    payload.source_session_name,
                    shared_session.current_session().is_some()
                ));
                let Some(session) = shared_session.current_session() else {
                    ERROR_LOG.log("[diag-bug] forward_host_output_to_shared_session: TargetExited has NO session, skipping".to_string());
                    continue;
                };
                ERROR_LOG.log(
                    "[diag-bug] forward_host_output_to_shared_session: calling send_target_exited on session"
                        .to_string(),
                );
                if session
                    .send_target_exited(
                        &payload.transport_session_id,
                        payload.source_session_name.as_deref(),
                    )
                    .is_err()
                {
                    ERROR_LOG.log("[diag-bug] forward_host_output_to_shared_session: send_target_exited FAILED, disconnecting".to_string());
                    shared_session.disconnect_session(&session);
                    mark_live_routes_offline(
                        &shared_session.publication_runtime,
                        &shared_session.routes,
                    );
                } else {
                    ERROR_LOG.log("[diag-bug] forward_host_output_to_shared_session: send_target_exited succeeded".to_string());
                }
            }
            other => {
                return Err(RemoteNodeSessionError::new(format!(
                    "unexpected live authority host payload `{}`",
                    other.message_type()
                )));
            }
        }
    }
    Ok(())
}

pub(super) fn authority_command_envelope(
    command: RemoteAuthorityCommand,
) -> ProtocolEnvelope<ControlPlanePayload> {
    let session_id = match &command {
        RemoteAuthorityCommand::OpenMirror(payload) => Some(payload.session_id.clone()),
        RemoteAuthorityCommand::CloseMirror(payload) => Some(payload.session_id.clone()),
        RemoteAuthorityCommand::RawPtyInput(payload) => Some(payload.session_id.clone()),
        RemoteAuthorityCommand::ApplyResize(payload) => Some(payload.session_id.clone()),
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
    };
    ProtocolEnvelope {
        protocol_version: REMOTE_PROTOCOL_VERSION.to_string(),
        message_id: format!("live-authority-{}", now_rfc3339_like()),
        message_type: payload.message_type(),
        timestamp: now_rfc3339_like(),
        sender_id: "waitagent-live-authority-owner".to_string(),
        correlation_id: None,
        session_id,
        target_id: None,
        attachment_id: None,
        console_id: None,
        payload,
    }
}

fn stable_socket_hash(values: &[&str]) -> String {
    let mut hash = 0xcbf29ce484222325_u64;
    for value in values {
        for byte in value.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
    }
    format!("{hash:016x}")
}

fn now_rfc3339_like() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!("{millis}Z")
}
