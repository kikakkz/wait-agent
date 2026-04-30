use crate::cli::{RemoteNetworkConfig, RemoteTargetPublicationSenderCommand};
use crate::domain::session_catalog::{
    ManagedSessionAddress, ManagedSessionRecord, ManagedSessionTaskState, SessionAvailability,
};
use crate::domain::workspace::WorkspaceSessionRole;
use crate::infra::remote_protocol::{
    ControlPlanePayload, ProtocolEnvelope, REMOTE_PROTOCOL_VERSION,
};
use crate::infra::remote_transport_codec::{
    read_control_plane_envelope, write_control_plane_envelope,
};
use crate::lifecycle::LifecycleError;
use crate::runtime::remote_authority_transport_runtime::RemoteAuthorityCommand;
use crate::runtime::remote_node_session_runtime::{
    RemoteNodeSessionError, RemoteNodeSessionRuntime,
};
use crate::runtime::remote_node_transport_runtime::{read_client_hello, write_server_hello};
use crate::runtime::remote_target_publication_runtime::{
    drain_pending_publication_sender_commands, read_publication_sender_command,
    remote_target_publication_sender_socket_path, PublicationSenderCommand,
    RemoteTargetPublicationRuntime,
};
use crate::runtime::remote_target_publication_transport_runtime::RemoteTargetPublicationTransportRuntime;
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

const LIVE_AUTHORITY_SERVER_ID: &str = "waitagent-live-authority-owner";
const SHARED_AUTHORITY_RECONNECT_BASE_DELAY: Duration = Duration::from_millis(100);
const SHARED_AUTHORITY_RECONNECT_MAX_DELAY: Duration = Duration::from_secs(1);

struct LiveSessionRoute {
    socket_name: String,
    target_session_name: String,
    authority_id: String,
    target_id: String,
    transport_session_id: String,
    socket_path: PathBuf,
    running: Arc<AtomicBool>,
    writer: Arc<Mutex<Option<UnixStream>>>,
}

#[derive(Clone)]
struct PendingExit {
    target_session_name: String,
    transport_session_id: String,
}

#[derive(Clone)]
struct SharedAuthoritySession {
    authority_id: String,
    transport_socket_path: String,
    publication_runtime: RemoteTargetPublicationRuntime,
    network: RemoteNetworkConfig,
    running: Arc<AtomicBool>,
    owner_started: Arc<AtomicBool>,
    session: Arc<Mutex<Option<Arc<RemoteNodeSessionRuntime>>>>,
    routes: Arc<Mutex<HashMap<String, Arc<LiveSessionRoute>>>>,
    pending_exits: Arc<Mutex<HashMap<String, PendingExit>>>,
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

pub struct RemoteNodeSessionOwnerRuntime {
    publication_runtime: RemoteTargetPublicationRuntime,
    network: RemoteNetworkConfig,
}

impl RemoteNodeSessionOwnerRuntime {
    pub fn from_build_env() -> Result<Self, LifecycleError> {
        Self::from_build_env_with_network(RemoteNetworkConfig::default())
    }

    pub fn from_build_env_with_network(
        network: RemoteNetworkConfig,
    ) -> Result<Self, LifecycleError> {
        Ok(Self {
            publication_runtime: RemoteTargetPublicationRuntime::from_build_env_with_network(
                network.clone(),
            )?,
            network,
        })
    }

    pub fn run_publication_sender(
        &self,
        command: RemoteTargetPublicationSenderCommand,
    ) -> Result<(), LifecycleError> {
        self.publication_runtime
            .ensure_publication_server_running(&command.socket_name)?;
        let socket_path = remote_target_publication_sender_socket_path(&command.socket_name);
        if socket_path.exists() {
            let _ = fs::remove_file(&socket_path);
        }
        let listener = UnixListener::bind(&socket_path).map_err(|error| {
            LifecycleError::Io(
                "failed to start remote node session owner".to_string(),
                error,
            )
        })?;
        let publication_socket_path =
            crate::runtime::remote_target_publication_transport_runtime::remote_target_publication_socket_path(
                &command.socket_name,
            );
        let mut transports = HashMap::<String, RemoteTargetPublicationTransportRuntime>::new();
        let mut live_sessions = HashMap::<String, Arc<LiveSessionRoute>>::new();
        let mut authority_sessions = HashMap::<String, SharedAuthoritySession>::new();
        for accepted in listener.incoming() {
            let Ok(mut stream) = accepted else {
                break;
            };
            let Ok(first_command) = read_publication_sender_command(&mut stream) else {
                continue;
            };
            let mut commands: Vec<PublicationSenderCommand> = vec![first_command];
            drain_pending_publication_sender_commands(&listener, &mut commands)?;
            reap_inactive_authority_sessions(&mut authority_sessions);
            for sender_command in commands {
                match sender_command {
                    PublicationSenderCommand::RegisterLiveSession {
                        target_session_name,
                        authority_id,
                        target_id,
                        transport_socket_path,
                    } => {
                        ensure_live_session_route(
                            &command.socket_name,
                            &target_session_name,
                            &authority_id,
                            &target_id,
                            &transport_socket_path,
                            &self.network,
                            &self.publication_runtime,
                            &mut live_sessions,
                            &mut authority_sessions,
                        )?;
                    }
                    PublicationSenderCommand::UnregisterLiveSession {
                        target_session_name,
                    } => {
                        stop_live_session_route(
                            &target_session_name,
                            &mut live_sessions,
                            &mut authority_sessions,
                        );
                    }
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
                        if let Some(source_session_name) = source_session_name.as_deref() {
                            let live_command = PublicationSenderCommand::PublishTarget {
                                authority_id: authority_id.clone(),
                                transport_session_id: transport_session_id.clone(),
                                source_session_name: Some(source_session_name.to_string()),
                                selector: selector.clone(),
                                availability,
                                session_role,
                                workspace_key: workspace_key.clone(),
                                command_name: command_name.clone(),
                                current_path: current_path.clone(),
                                attached_clients,
                                window_count,
                                task_state,
                            };
                            if dispatch_live_publication(
                                source_session_name,
                                &live_command,
                                &mut live_sessions,
                                &mut authority_sessions,
                            ) {
                                continue;
                            }
                        }
                        dispatch_publication_sender_command(
                            &publication_socket_path,
                            &mut transports,
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
                            },
                        )?;
                    }
                    PublicationSenderCommand::ExitTarget {
                        authority_id,
                        transport_session_id,
                        source_session_name,
                    } => {
                        if let Some(source_session_name) = source_session_name.as_deref() {
                            let live_command = PublicationSenderCommand::ExitTarget {
                                authority_id: authority_id.clone(),
                                transport_session_id: transport_session_id.clone(),
                                source_session_name: Some(source_session_name.to_string()),
                            };
                            if dispatch_live_publication(
                                source_session_name,
                                &live_command,
                                &mut live_sessions,
                                &mut authority_sessions,
                            ) {
                                continue;
                            }
                        }
                        dispatch_publication_sender_command(
                            &publication_socket_path,
                            &mut transports,
                            PublicationSenderCommand::ExitTarget {
                                authority_id,
                                transport_session_id,
                                source_session_name,
                            },
                        )?;
                    }
                }
            }
        }
        for target_session_name in live_sessions.keys().cloned().collect::<Vec<_>>() {
            stop_live_session_route(
                &target_session_name,
                &mut live_sessions,
                &mut authority_sessions,
            );
        }
        Ok(())
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

fn dispatch_publication_sender_command(
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

pub(crate) fn spawn_live_authority_session_bridge(
    socket_path: PathBuf,
    session: Arc<RemoteNodeSessionRuntime>,
    running: Arc<AtomicBool>,
) {
    thread::spawn(move || {
        let Ok(listener) = bind_live_authority_listener(&socket_path) else {
            return;
        };
        while running.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((stream, _)) => {
                    let _ = bridge_live_authority_stream(stream, session.clone(), running.clone());
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

fn ensure_live_session_route(
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
    });
    spawn_live_authority_route_listener(shared_session.clone(), route.clone());
    shared_session
        .routes
        .lock()
        .expect("shared authority routes mutex should not be poisoned")
        .insert(target_session_name.to_string(), route.clone());
    start_shared_authority_command_dispatcher(shared_session.clone());
    live_sessions.insert(target_session_name.to_string(), route);
    Ok(())
}

fn stop_live_session_route(
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
        let _ = UnixStream::connect(&route.socket_path);
        let _ = fs::remove_file(&route.socket_path);
        let mut remove_authority = false;
        if let Some(shared_session) = authority_sessions.get(&route.authority_id) {
            if shared_session.current_session().is_none() {
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

fn dispatch_live_publication(
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
                    reconnect_attempt = 0;
                    Arc::new(session)
                }
                Err(_) => {
                    thread::sleep(shared_authority_reconnect_delay(reconnect_attempt));
                    reconnect_attempt = reconnect_attempt.saturating_add(1);
                    continue;
                }
            };
            shared_session.replace_session(Some(session.clone()));
            if (reconnect_attempt > 0 || shared_session.has_pending_exits())
                && !restore_shared_authority_state(&shared_session, &session)
            {
                reconnect_attempt = reconnect_attempt.saturating_add(1);
                thread::sleep(shared_authority_reconnect_delay(reconnect_attempt));
                continue;
            }
            while shared_session.is_running() {
                if shared_session.stop_if_idle() {
                    break;
                }
                match session.recv_authority_command() {
                    Ok(command) => {
                        let _ = dispatch_authority_command_to_live_route(
                            &shared_session.routes,
                            &command,
                        );
                    }
                    Err(_) => {
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
        let _ = shared_session
            .publication_runtime
            .signal_source_session_refresh(&route.socket_name, &route.target_session_name);
    }
    true
}

fn shared_authority_reconnect_delay(attempt: u32) -> Duration {
    let multiplier = 1_u32 << attempt.min(4);
    std::cmp::min(
        SHARED_AUTHORITY_RECONNECT_BASE_DELAY.saturating_mul(multiplier),
        SHARED_AUTHORITY_RECONNECT_MAX_DELAY,
    )
}

fn reap_inactive_authority_sessions(
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

fn dispatch_authority_command_to_live_route(
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
        return Ok(());
    };
    if let Err(error) =
        write_control_plane_envelope(writer, &authority_command_envelope(command.clone()))
    {
        let _ = writer.shutdown(Shutdown::Both);
        *writer_guard = None;
        return Err(RemoteNodeSessionError::new(error.to_string()));
    }
    Ok(())
}

fn authority_command_target_id(command: &RemoteAuthorityCommand) -> &str {
    match command {
        RemoteAuthorityCommand::TargetInput(payload) => payload.target_id.as_str(),
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
        let _ = UnixStream::connect(&route.socket_path);
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

fn bridge_shared_live_authority_stream(
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
        *writer_guard = Some(host_stream.try_clone()?);
    }
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
        let command = match session.recv_authority_command() {
            Ok(command) => command,
            Err(error) => {
                let _ = host_stream.shutdown(Shutdown::Both);
                let _ = forward_host.join();
                return Err(error);
            }
        };
        write_control_plane_envelope(&mut host_stream, &authority_command_envelope(command))?;
    }
    let _ = host_stream.shutdown(Shutdown::Both);
    let _ = forward_host.join();
    Ok(())
}

fn forward_host_output_to_session(
    mut host_reader: UnixStream,
    session: Arc<RemoteNodeSessionRuntime>,
    running: Arc<AtomicBool>,
) -> Result<(), RemoteNodeSessionError> {
    while running.load(Ordering::Relaxed) {
        let envelope = read_control_plane_envelope(&mut host_reader)?;
        match envelope.payload {
            ControlPlanePayload::TargetOutput(payload) => {
                session.send_target_output(
                    &payload.session_id,
                    &payload.target_id,
                    payload.output_seq,
                    payload.stream,
                    payload.bytes_base64,
                )?;
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

fn forward_host_output_to_shared_session(
    mut host_reader: UnixStream,
    shared_session: SharedAuthoritySession,
    running: Arc<AtomicBool>,
) -> Result<(), RemoteNodeSessionError> {
    while running.load(Ordering::Relaxed) {
        let envelope = read_control_plane_envelope(&mut host_reader)?;
        match envelope.payload {
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
                        payload.bytes_base64,
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

fn authority_command_envelope(
    command: RemoteAuthorityCommand,
) -> ProtocolEnvelope<ControlPlanePayload> {
    let session_id = match &command {
        RemoteAuthorityCommand::TargetInput(payload) => Some(payload.session_id.clone()),
        RemoteAuthorityCommand::ApplyResize(payload) => Some(payload.session_id.clone()),
    };
    let payload = match command {
        RemoteAuthorityCommand::TargetInput(payload) => ControlPlanePayload::TargetInput(payload),
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

#[cfg(test)]
mod tests {
    use super::{
        dispatch_publication_sender_command, ensure_live_session_route,
        live_authority_session_socket_path, SharedAuthoritySession,
    };
    use crate::cli::RemoteNetworkConfig;
    use crate::infra::remote_protocol::{
        ApplyResizePayload, ClientHelloPayload, ControlPlanePayload, NodeSessionChannel,
        NodeSessionEnvelope, ProtocolEnvelope, TargetExitedPayload, TargetInputPayload,
        TargetPublishedPayload, REMOTE_PROTOCOL_VERSION,
    };
    use crate::infra::remote_transport_codec::{
        read_control_plane_envelope, read_node_session_envelope, write_node_session_envelope,
    };
    use crate::runtime::remote_authority_transport_runtime::{
        RemoteAuthorityCommand, RemoteAuthorityTransportRuntime,
    };
    use crate::runtime::remote_node_transport_runtime::{
        read_client_hello, write_server_hello, NODE_TRANSPORT_CLIENT_VERSION,
    };
    use crate::runtime::remote_target_publication_runtime::{
        PublicationSenderCommand, RemoteTargetPublicationRuntime,
    };
    use crate::runtime::remote_target_publication_transport_runtime::RemoteTargetPublicationTransportRuntime;
    use std::collections::HashMap;
    use std::fs;
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    #[test]
    fn owner_runtime_reuses_cached_publication_transport_for_publish_and_exit() {
        let socket_path = test_socket_path("owner-publication-cache");
        let listener = UnixListener::bind(&socket_path).expect("listener should bind");
        let accept_thread = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("listener should accept");
            match read_control_plane_envelope(&mut stream)
                .expect("client hello should decode")
                .payload
            {
                ControlPlanePayload::ClientHello(ClientHelloPayload {
                    node_id,
                    client_version,
                }) => {
                    assert_eq!(node_id, "peer-a");
                    assert_eq!(client_version, NODE_TRANSPORT_CLIENT_VERSION);
                }
                other => panic!("unexpected hello payload: {other:?}"),
            }
            write_server_hello(&mut stream, "waitagent-publication")
                .expect("server hello should encode");
            let published =
                read_node_session_envelope(&mut stream).expect("publish envelope should decode");
            let exited =
                read_node_session_envelope(&mut stream).expect("exit envelope should decode");
            (published, exited)
        });

        let mut transports = HashMap::<String, RemoteTargetPublicationTransportRuntime>::new();
        dispatch_publication_sender_command(
            &socket_path,
            &mut transports,
            PublicationSenderCommand::PublishTarget {
                authority_id: "peer-a".to_string(),
                transport_session_id: "shell-1".to_string(),
                source_session_name: Some("target-host-1".to_string()),
                selector: Some("wk:shell".to_string()),
                availability: "online",
                session_role: Some("target-host"),
                workspace_key: Some("wk-1".to_string()),
                command_name: Some("codex".to_string()),
                current_path: Some("/tmp/demo".to_string()),
                attached_clients: 2,
                window_count: 3,
                task_state: "confirm",
            },
        )
        .expect("publish command should route through owner transport cache");
        dispatch_publication_sender_command(
            &socket_path,
            &mut transports,
            PublicationSenderCommand::ExitTarget {
                authority_id: "peer-a".to_string(),
                transport_session_id: "shell-1".to_string(),
                source_session_name: Some("target-host-1".to_string()),
            },
        )
        .expect("exit command should reuse cached owner transport");

        assert_eq!(transports.len(), 1);

        let (published, exited) = accept_thread
            .join()
            .expect("accept thread should join cleanly");
        assert_eq!(published.channel, NodeSessionChannel::Publication);
        match published.envelope.payload {
            ControlPlanePayload::TargetPublished(TargetPublishedPayload {
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
            }) => {
                assert_eq!(transport_session_id, "shell-1");
                assert_eq!(source_session_name.as_deref(), Some("target-host-1"));
                assert_eq!(selector.as_deref(), Some("wk:shell"));
                assert_eq!(availability, "online");
                assert_eq!(session_role, Some("target-host"));
                assert_eq!(workspace_key.as_deref(), Some("wk-1"));
                assert_eq!(command_name.as_deref(), Some("codex"));
                assert_eq!(current_path.as_deref(), Some("/tmp/demo"));
                assert_eq!(attached_clients, 2);
                assert_eq!(window_count, 3);
                assert_eq!(task_state, "confirm");
            }
            other => panic!("unexpected publish payload: {other:?}"),
        }

        assert_eq!(exited.channel, NodeSessionChannel::Publication);
        assert_eq!(
            exited.envelope.payload,
            ControlPlanePayload::TargetExited(TargetExitedPayload {
                transport_session_id: "shell-1".to_string(),
                source_session_name: Some("target-host-1".to_string()),
            })
        );

        let _ = fs::remove_file(&socket_path);
    }

    #[test]
    fn shared_authority_session_reuses_one_node_connection_and_routes_by_target_id() {
        let socket_name = "wa-shared";
        let target_session_a = "target-sa";
        let target_session_b = "target-sb";
        let target_id_a = "remote-peer:peer-a:target-sa";
        let target_id_b = "remote-peer:peer-a:target-sb";
        let transport_socket_path = test_socket_path("shared-authority-session");
        let listener = UnixListener::bind(&transport_socket_path).expect("listener should bind");
        listener
            .set_nonblocking(true)
            .expect("listener should allow nonblocking accept");
        let accept_count = Arc::new(AtomicUsize::new(0));
        let server_running = Arc::new(AtomicBool::new(true));
        let server_stream = Arc::new(Mutex::new(None::<std::os::unix::net::UnixStream>));
        let server_thread = {
            let accept_count = accept_count.clone();
            let server_running = server_running.clone();
            let server_stream = server_stream.clone();
            thread::spawn(move || {
                while server_running.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            accept_count.fetch_add(1, Ordering::Relaxed);
                            let node_id =
                                read_client_hello(&mut stream).expect("client hello should decode");
                            assert_eq!(node_id, "peer-a");
                            write_server_hello(&mut stream, "waitagent-remote-node-session")
                                .expect("server hello should encode");
                            let mut shared_stream = server_stream
                                .lock()
                                .expect("server stream mutex should not be poisoned");
                            if shared_stream.is_none() {
                                *shared_stream = Some(
                                    stream
                                        .try_clone()
                                        .expect("accepted stream should clone for test server"),
                                );
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(10));
                        }
                        Err(_) => break,
                    }
                }
            })
        };

        let mut live_sessions = HashMap::new();
        let mut authority_sessions = HashMap::<String, SharedAuthoritySession>::new();
        let publication_runtime =
            RemoteTargetPublicationRuntime::from_build_env().expect("publication runtime");
        ensure_live_session_route(
            socket_name,
            target_session_a,
            "peer-a",
            target_id_a,
            transport_socket_path.to_string_lossy().as_ref(),
            &RemoteNetworkConfig::default(),
            &publication_runtime,
            &mut live_sessions,
            &mut authority_sessions,
        )
        .expect("first live session route should register");
        ensure_live_session_route(
            socket_name,
            target_session_b,
            "peer-a",
            target_id_b,
            transport_socket_path.to_string_lossy().as_ref(),
            &RemoteNetworkConfig::default(),
            &publication_runtime,
            &mut live_sessions,
            &mut authority_sessions,
        )
        .expect("second live session route should reuse authority session");

        wait_for_ready_socket(&live_authority_session_socket_path(
            socket_name,
            target_session_a,
        ));
        wait_for_ready_socket(&live_authority_session_socket_path(
            socket_name,
            target_session_b,
        ));
        let transport_a = connect_authority_transport_with_retry(
            live_authority_session_socket_path(socket_name, target_session_a),
            "peer-a",
        );
        let transport_b = connect_authority_transport_with_retry(
            live_authority_session_socket_path(socket_name, target_session_b),
            "peer-a",
        );

        wait_for_condition(Duration::from_secs(1), || {
            accept_count.load(Ordering::Relaxed) == 1
                && server_stream
                    .lock()
                    .expect("server stream mutex should not be poisoned")
                    .is_some()
        });
        assert_eq!(accept_count.load(Ordering::Relaxed), 1);

        {
            let mut server_stream = server_stream
                .lock()
                .expect("server stream mutex should not be poisoned");
            let stream = server_stream
                .as_mut()
                .expect("shared authority stream should be available");
            write_node_session_envelope(
                stream,
                &NodeSessionEnvelope {
                    channel: NodeSessionChannel::Authority,
                    envelope: target_input_envelope(
                        target_id_a,
                        "attach-a",
                        "console-a",
                        7,
                        "YQ==",
                    ),
                },
            )
            .expect("target input should encode");
            write_node_session_envelope(
                stream,
                &NodeSessionEnvelope {
                    channel: NodeSessionChannel::Authority,
                    envelope: apply_resize_envelope(target_id_b, 80, 24),
                },
            )
            .expect("resize command should encode");
        }

        match recv_command_with_timeout(transport_a, Duration::from_secs(1))
            .expect("target-a command should arrive")
        {
            RemoteAuthorityCommand::TargetInput(payload) => {
                assert_eq!(payload.target_id, target_id_a);
                assert_eq!(payload.input_seq, 7);
                assert_eq!(payload.bytes_base64, "YQ==");
            }
            other => panic!("unexpected target-a authority command: {other:?}"),
        }
        match recv_command_with_timeout(transport_b, Duration::from_secs(1))
            .expect("target-b command should arrive")
        {
            RemoteAuthorityCommand::ApplyResize(payload) => {
                assert_eq!(payload.target_id, target_id_b);
                assert_eq!(payload.cols, 80);
                assert_eq!(payload.rows, 24);
            }
            other => panic!("unexpected target-b authority command: {other:?}"),
        }

        // This harness intentionally avoids joining the background bridge threads.
        // The test only needs to prove one shared authority session and target-id routing.
        drop((
            live_sessions,
            authority_sessions,
            server_running,
            server_thread,
        ));
    }

    #[test]
    fn shared_authority_session_reconnects_without_dropping_local_authority_bridge() {
        let socket_name = "wa-reconnect";
        let target_session_name = "target-r";
        let target_id = "remote-peer:peer-a:target-r";
        let transport_socket_path = test_socket_path("shared-authority-reconnect");
        let listener = UnixListener::bind(&transport_socket_path).expect("listener should bind");
        listener
            .set_nonblocking(true)
            .expect("listener should allow nonblocking accept");
        let accept_count = Arc::new(AtomicUsize::new(0));
        let server_running = Arc::new(AtomicBool::new(true));
        let server_stream = Arc::new(Mutex::new(None::<std::os::unix::net::UnixStream>));
        let server_thread = {
            let accept_count = accept_count.clone();
            let server_running = server_running.clone();
            let server_stream = server_stream.clone();
            thread::spawn(move || {
                while server_running.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            accept_count.fetch_add(1, Ordering::Relaxed);
                            let node_id =
                                read_client_hello(&mut stream).expect("client hello should decode");
                            assert_eq!(node_id, "peer-a");
                            write_server_hello(&mut stream, "waitagent-remote-node-session")
                                .expect("server hello should encode");
                            let mut shared_stream = server_stream
                                .lock()
                                .expect("server stream mutex should not be poisoned");
                            *shared_stream = Some(
                                stream
                                    .try_clone()
                                    .expect("accepted stream should clone for test server"),
                            );
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(10));
                        }
                        Err(_) => break,
                    }
                }
            })
        };

        let mut live_sessions = HashMap::new();
        let mut authority_sessions = HashMap::<String, SharedAuthoritySession>::new();
        let publication_runtime =
            RemoteTargetPublicationRuntime::from_build_env().expect("publication runtime");
        ensure_live_session_route(
            socket_name,
            target_session_name,
            "peer-a",
            target_id,
            transport_socket_path.to_string_lossy().as_ref(),
            &RemoteNetworkConfig::default(),
            &publication_runtime,
            &mut live_sessions,
            &mut authority_sessions,
        )
        .expect("live session route should register");

        wait_for_ready_socket(&live_authority_session_socket_path(
            socket_name,
            target_session_name,
        ));
        let transport_a = Arc::new(connect_authority_transport_with_retry(
            live_authority_session_socket_path(socket_name, target_session_name),
            "peer-a",
        ));

        wait_for_condition(Duration::from_secs(1), || {
            accept_count.load(Ordering::Relaxed) == 1
                && server_stream
                    .lock()
                    .expect("server stream mutex should not be poisoned")
                    .is_some()
        });
        {
            let mut server_stream = server_stream
                .lock()
                .expect("server stream mutex should not be poisoned");
            let stream = server_stream
                .as_mut()
                .expect("shared authority stream should be available");
            write_node_session_envelope(
                stream,
                &NodeSessionEnvelope {
                    channel: NodeSessionChannel::Authority,
                    envelope: target_input_envelope(target_id, "attach-a", "console-a", 1, "YQ=="),
                },
            )
            .expect("initial target input should encode");
        }
        match recv_shared_command_with_timeout(transport_a.clone(), Duration::from_secs(1))
            .expect("initial target-a command should arrive")
        {
            RemoteAuthorityCommand::TargetInput(payload) => {
                assert_eq!(payload.target_id, target_id);
                assert_eq!(payload.input_seq, 1);
            }
            other => panic!("unexpected initial authority command: {other:?}"),
        }

        {
            let mut server_stream = server_stream
                .lock()
                .expect("server stream mutex should not be poisoned");
            let stream = server_stream
                .take()
                .expect("first shared authority stream should be available");
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }

        wait_for_condition(Duration::from_secs(2), || {
            accept_count.load(Ordering::Relaxed) >= 2
                && server_stream
                    .lock()
                    .expect("server stream mutex should not be poisoned")
                    .is_some()
        });
        {
            let mut server_stream = server_stream
                .lock()
                .expect("server stream mutex should not be poisoned");
            let stream = server_stream
                .as_mut()
                .expect("reconnected shared authority stream should be available");
            write_node_session_envelope(
                stream,
                &NodeSessionEnvelope {
                    channel: NodeSessionChannel::Authority,
                    envelope: target_input_envelope(target_id, "attach-a", "console-a", 2, "Yg=="),
                },
            )
            .expect("reconnected target input should encode");
        }
        match recv_shared_command_with_timeout(transport_a, Duration::from_secs(1))
            .expect("reconnected target-a command should arrive")
        {
            RemoteAuthorityCommand::TargetInput(payload) => {
                assert_eq!(payload.target_id, target_id);
                assert_eq!(payload.input_seq, 2);
                assert_eq!(payload.bytes_base64, "Yg==");
            }
            other => panic!("unexpected reconnected authority command: {other:?}"),
        }

        drop((
            live_sessions,
            authority_sessions,
            server_running,
            server_thread,
        ));
    }

    fn test_socket_path(label: &str) -> PathBuf {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock should be after unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "waitagent-remote-node-session-owner-test-{}-{}-{label}.sock",
            std::process::id(),
            now
        ))
    }

    fn target_input_envelope(
        target_id: &str,
        attachment_id: &str,
        console_id: &str,
        input_seq: u64,
        bytes_base64: &str,
    ) -> ProtocolEnvelope<ControlPlanePayload> {
        let payload = ControlPlanePayload::TargetInput(TargetInputPayload {
            attachment_id: attachment_id.to_string(),
            session_id: target_id
                .splitn(3, ':')
                .nth(2)
                .unwrap_or(target_id)
                .to_string(),
            target_id: target_id.to_string(),
            console_id: console_id.to_string(),
            console_host_id: "host-a".to_string(),
            input_seq,
            bytes_base64: bytes_base64.to_string(),
        });
        ProtocolEnvelope {
            protocol_version: REMOTE_PROTOCOL_VERSION.to_string(),
            message_id: format!("target-input-{input_seq}"),
            message_type: payload.message_type(),
            timestamp: "1Z".to_string(),
            sender_id: "server".to_string(),
            correlation_id: None,
            session_id: Some(
                target_id
                    .splitn(3, ':')
                    .nth(2)
                    .unwrap_or(target_id)
                    .to_string(),
            ),
            target_id: Some(target_id.to_string()),
            attachment_id: Some(attachment_id.to_string()),
            console_id: Some(console_id.to_string()),
            payload,
        }
    }

    fn apply_resize_envelope(
        target_id: &str,
        cols: usize,
        rows: usize,
    ) -> ProtocolEnvelope<ControlPlanePayload> {
        let payload = ControlPlanePayload::ApplyResize(ApplyResizePayload {
            session_id: target_id
                .splitn(3, ':')
                .nth(2)
                .unwrap_or(target_id)
                .to_string(),
            target_id: target_id.to_string(),
            resize_epoch: 1,
            resize_authority_console_id: "console-b".to_string(),
            cols,
            rows,
        });
        ProtocolEnvelope {
            protocol_version: REMOTE_PROTOCOL_VERSION.to_string(),
            message_id: "resize-1".to_string(),
            message_type: payload.message_type(),
            timestamp: "1Z".to_string(),
            sender_id: "server".to_string(),
            correlation_id: None,
            session_id: Some(
                target_id
                    .splitn(3, ':')
                    .nth(2)
                    .unwrap_or(target_id)
                    .to_string(),
            ),
            target_id: Some(target_id.to_string()),
            attachment_id: None,
            console_id: Some("console-b".to_string()),
            payload,
        }
    }

    fn wait_for_ready_socket(socket_path: &PathBuf) {
        wait_for_condition(Duration::from_secs(1), || socket_path.exists());
    }

    fn wait_for_condition(timeout: Duration, predicate: impl Fn() -> bool) {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if predicate() {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(predicate(), "condition did not become true within timeout");
    }

    fn recv_command_with_timeout(
        transport: RemoteAuthorityTransportRuntime,
        timeout: Duration,
    ) -> Result<RemoteAuthorityCommand, String> {
        let (tx, rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(transport.recv_command().map_err(|error| error.to_string()));
        });
        rx.recv_timeout(timeout)
            .map_err(|_| "authority command timed out".to_string())?
    }

    fn recv_shared_command_with_timeout(
        transport: Arc<RemoteAuthorityTransportRuntime>,
        timeout: Duration,
    ) -> Result<RemoteAuthorityCommand, String> {
        let (tx, rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(transport.recv_command().map_err(|error| error.to_string()));
        });
        rx.recv_timeout(timeout)
            .map_err(|_| "authority command timed out".to_string())?
    }

    fn connect_authority_transport_with_retry(
        socket_path: PathBuf,
        node_id: &str,
    ) -> RemoteAuthorityTransportRuntime {
        let start = Instant::now();
        loop {
            match RemoteAuthorityTransportRuntime::connect(&socket_path, node_id) {
                Ok(transport) => return transport,
                Err(error) if start.elapsed() < Duration::from_secs(1) => {
                    let _ = error;
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("target authority bridge should connect: {error:?}"),
            }
        }
    }
}
