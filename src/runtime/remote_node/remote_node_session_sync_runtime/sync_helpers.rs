use crate::cli::RemoteAuthorityTargetHostCommand;
use crate::cli::{prepend_global_network_args, RemoteNetworkConfig};
use crate::domain::agent_detector::SHELL_NAMES;
use crate::domain::session_catalog::{ManagedSessionRecord, ManagedSessionTaskState};
use crate::infra::published_target_store::PublishedTargetStore;
use crate::infra::remote_grpc_proto::v1::node_session_envelope::Body;
use crate::infra::remote_grpc_proto::v1::{
    NodeSessionEnvelope as GrpcNodeSessionEnvelope, RouteContext, TargetExited, TargetPublished,
};
use crate::infra::remote_grpc_transport::{
    OutboundNodeSessionRequest, RemoteNodeSessionHandle, RemoteNodeTransportEvent,
};
use crate::infra::remote_protocol::{ControlPlanePayload, NodeSessionChannel, ProtocolEnvelope};
use crate::infra::remote_transport_codec::{
    read_control_plane_envelope, write_control_plane_envelope,
};
use crate::infra::tmux::{
    EmbeddedTmuxBackend, TmuxChromeGateway, TmuxSessionGateway, TmuxSessionName, TmuxSocketName,
    TmuxWorkspaceHandle,
};
use crate::lifecycle::LifecycleError;
use crate::runtime::remote_authority_target_host_runtime::RemoteAuthorityTargetHostRuntime;
use crate::runtime::remote_authority_transport_runtime::RemoteAuthorityCommand;
use crate::runtime::remote_node_session_owner_runtime::live_authority_session_socket_path;
use crate::runtime::remote_node_session_runtime::{
    map_inbound_grpc_authority_event, map_outbound_grpc_envelope,
};
use crate::runtime::remote_node_transport_runtime::{read_client_hello, write_server_hello};
use std::collections::HashMap;
use std::io;
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::{
    LocalSessionCatalog, NoopAuthorityPublicationGateway, OutboundRemoteNodeTransport,
    SessionSyncAuthorityHost, SessionSyncAuthorityManager, LIVE_AUTHORITY_SERVER_ID,
    SESSION_SYNC_AUTHORITY_ID, SESSION_SYNC_RAW_INPUT_QUIET_WINDOW, WAITAGENT_ACTIVE_TARGET_OPTION,
};

pub(super) fn run_remote_session_sync_loop<G, T>(
    gateway: G,
    transport: T,
    _network: RemoteNetworkConfig,
    node_id: String,
    endpoint_uri: String,
    poll_interval: Duration,
    reconnect_delay: Duration,
    stop_rx: mpsc::Receiver<()>,
) where
    G: LocalSessionCatalog,
    T: OutboundRemoteNodeTransport,
{
    let mut next_message_id = 0_u64;
    loop {
        if should_stop(&stop_rx) {
            return;
        }

        let (event_tx, event_rx) = mpsc::channel();
        let _transport_guard = match transport.connect_outbound(
            OutboundNodeSessionRequest {
                node_id: node_id.clone(),
                endpoint_uri: endpoint_uri.clone(),
            },
            event_tx,
        ) {
            Ok(guard) => guard,
            Err(_) => {
                if wait_or_stop(&stop_rx, reconnect_delay) {
                    return;
                }
                continue;
            }
        };

        let mut active_session = None;
        let mut synced_sessions = HashMap::<String, ManagedSessionRecord>::new();
        let mut authority_manager = SessionSyncAuthorityManager::new();
        let mut should_reconnect = false;
        let mut next_sync_at = Instant::now() + poll_interval;
        let mut raw_input_quiet_until: Option<Instant> = None;

        while !should_reconnect {
            let wait_duration = next_sync_at.saturating_duration_since(Instant::now());
            if let Ok(event) = event_rx.recv_timeout(wait_duration) {
                let outcome =
                    handle_transport_event(event, &mut active_session, &mut authority_manager);
                should_reconnect |= outcome.should_reconnect;
                if outcome.raw_pty_input {
                    raw_input_quiet_until =
                        Some(Instant::now() + SESSION_SYNC_RAW_INPUT_QUIET_WINDOW);
                }
                while let Ok(event) = event_rx.try_recv() {
                    let outcome =
                        handle_transport_event(event, &mut active_session, &mut authority_manager);
                    should_reconnect |= outcome.should_reconnect;
                    if outcome.raw_pty_input {
                        raw_input_quiet_until =
                            Some(Instant::now() + SESSION_SYNC_RAW_INPUT_QUIET_WINDOW);
                    }
                }
            }

            if should_stop(&stop_rx) {
                return;
            }

            let Some(session_handle) = active_session.as_ref() else {
                next_sync_at = Instant::now() + poll_interval;
                continue;
            };
            if Instant::now() < next_sync_at {
                continue;
            }
            if raw_input_quiet_until
                .map(|quiet_until| Instant::now() < quiet_until)
                .unwrap_or(false)
            {
                next_sync_at = raw_input_quiet_until.unwrap_or_else(Instant::now);
                continue;
            }
            if let Err(_) = sync_local_sessions(
                &gateway,
                &node_id,
                session_handle,
                &mut synced_sessions,
                &mut next_message_id,
            ) {
                should_reconnect = true;
            }
            next_sync_at = Instant::now() + poll_interval;
        }

        if wait_or_stop(&stop_rx, reconnect_delay) {
            return;
        }
        authority_manager.shutdown();
    }
}

#[derive(Debug, Default)]
pub(super) struct TransportEventOutcome {
    pub(super) should_reconnect: bool,
    pub(super) raw_pty_input: bool,
}

pub(super) fn handle_transport_event(
    event: RemoteNodeTransportEvent,
    active_session: &mut Option<RemoteNodeSessionHandle>,
    authority_manager: &mut SessionSyncAuthorityManager,
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
            let raw_pty_input = matches!(envelope.body.as_ref(), Some(Body::RawPtyInput(_)));
            if let Some(event) = map_inbound_grpc_authority_event(envelope) {
                authority_manager.handle_event(session_handle, event);
            }
            TransportEventOutcome {
                should_reconnect: false,
                raw_pty_input,
            }
        }
        RemoteNodeTransportEvent::SessionClosed { .. }
        | RemoteNodeTransportEvent::TransportFailed { .. } => {
            authority_manager.shutdown();
            *active_session = None;
            TransportEventOutcome {
                should_reconnect: true,
                raw_pty_input: false,
            }
        }
    }
}

pub(super) fn sync_local_sessions<G>(
    gateway: &G,
    node_id: &str,
    session_handle: &RemoteNodeSessionHandle,
    synced_sessions: &mut HashMap<String, ManagedSessionRecord>,
    next_message_id: &mut u64,
) -> Result<(), io::Error>
where
    G: LocalSessionCatalog,
{
    let local_sessions = match gateway.list_local_sessions() {
        Ok(sessions) => sessions,
        Err(_) => return Ok(()),
    };
    let current_sessions = local_sessions_by_local_id(local_sessions);
    let delta = compute_session_sync_delta(synced_sessions, &current_sessions);

    for session in &delta.publish {
        next_message_id_increment(next_message_id);
        session_handle
            .send(remote_session_published_envelope(
                node_id,
                session_handle.session_instance_id(),
                *next_message_id,
                session,
            ))
            .map_err(|error| io::Error::new(io::ErrorKind::BrokenPipe, error.to_string()))?;
    }

    for previous in &delta.exit {
        next_message_id_increment(next_message_id);
        session_handle
            .send(remote_session_exited_envelope(
                node_id,
                session_handle.session_instance_id(),
                *next_message_id,
                previous.address.session_id(),
            ))
            .map_err(|error| io::Error::new(io::ErrorKind::BrokenPipe, error.to_string()))?;
    }

    *synced_sessions = current_sessions;
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
            session.address.server_id() == socket_name && session.is_workspace_session()
        })
        .map(|session| {
            exported_session_record_for_socket(session, socket_name, published_target_store)
        })
        .collect()
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
        if should_overlay_active_target_runtime(active_target) {
            active_target.command_name = workspace_runtime.command_name.clone();
            active_target.current_path = workspace_runtime.current_path.clone();
            active_target.task_state = workspace_runtime.task_state;
        }
    }
    sessions
}

fn should_overlay_active_target_runtime(session: &ManagedSessionRecord) -> bool {
    session
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

pub(crate) fn local_sessions_by_local_id(
    sessions: Vec<ManagedSessionRecord>,
) -> HashMap<String, ManagedSessionRecord> {
    sessions
        .into_iter()
        .map(|session| (session.address.id().as_str().to_string(), session))
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
    cached_remote_target.command_name = live_target.command_name.clone();
    cached_remote_target.current_path = live_target.current_path.clone();
    cached_remote_target.task_state = live_target.task_state;
    cached_remote_target
}

#[derive(Debug)]
pub(crate) struct SessionSyncDelta {
    pub(crate) publish: Vec<ManagedSessionRecord>,
    pub(crate) exit: Vec<ManagedSessionRecord>,
}

pub(crate) fn compute_session_sync_delta(
    previous: &HashMap<String, ManagedSessionRecord>,
    current: &HashMap<String, ManagedSessionRecord>,
) -> SessionSyncDelta {
    let publish = current
        .iter()
        .filter_map(|(local_id, session)| {
            if previous
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
    let mut parts = target_id.splitn(3, ':');
    let _transport = parts.next()?;
    let _authority = parts.next()?;
    let session_name = parts.next()?;
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
    command: RemoteAuthorityTargetHostCommand,
) -> Result<(), LifecycleError> {
    let gateway = EmbeddedTmuxBackend::from_build_env().map_err(remote_session_sync_error)?;
    let current_executable = std::env::current_exe().map_err(|error| {
        LifecycleError::Io(
            "failed to locate current waitagent executable".to_string(),
            error,
        )
    })?;
    let runtime = RemoteAuthorityTargetHostRuntime::new(
        gateway,
        NoopAuthorityPublicationGateway,
        current_executable,
    );
    let authority_socket_path =
        live_authority_session_socket_path(&command.socket_name, &command.target_session_name);
    thread::spawn(move || {
        let _ = runtime.run_target_host(command);
        running.store(false, Ordering::Relaxed);
        if let Some(writer) = writer
            .lock()
            .expect("authority writer mutex should not be poisoned")
            .take()
        {
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
        let mut writer_guard = writer
            .lock()
            .expect("authority writer mutex should not be poisoned");
        if let Some(previous) = writer_guard.take() {
            let _ = previous.shutdown(Shutdown::Both);
        }
        *writer_guard = Some(host_stream.try_clone().map_err(remote_session_sync_error)?);
    }
    writer_ready.notify_all();
    let result = forward_host_output_to_session(host_reader, session_handle, running.clone());
    let _ = host_stream.shutdown(Shutdown::Both);
    let _ = writer
        .lock()
        .expect("authority writer mutex should not be poisoned")
        .take();
    result
}

fn forward_host_output_to_session(
    mut host_reader: UnixStream,
    session_handle: RemoteNodeSessionHandle,
    running: Arc<AtomicBool>,
) -> Result<(), LifecycleError> {
    while running.load(Ordering::Relaxed) {
        let envelope =
            read_control_plane_envelope(&mut host_reader).map_err(remote_session_sync_error)?;
        let grpc = map_outbound_grpc_envelope(
            session_handle.node_id(),
            NodeSessionChannel::Authority,
            &envelope,
        )
        .map_err(remote_session_sync_error)?;
        session_handle
            .send(grpc)
            .map_err(remote_session_sync_error)?;
    }
    Ok(())
}

pub(super) fn send_command_to_host(
    host: &SessionSyncAuthorityHost,
    command: RemoteAuthorityCommand,
) -> Result<(), LifecycleError> {
    let mut guard = host
        .writer
        .lock()
        .expect("authority writer mutex should not be poisoned");
    while guard.is_none() {
        if !host.running.load(Ordering::Relaxed) {
            break;
        }
        guard = host
            .writer_ready
            .wait_timeout(guard, Duration::from_secs(2))
            .expect("authority writer condvar should not be poisoned")
            .0;
    }
    if let Some(writer) = guard.as_mut() {
        write_control_plane_envelope(writer, &authority_command_envelope(command))
            .map_err(remote_session_sync_error)?;
        Ok(())
    } else {
        Err(LifecycleError::Protocol(format!(
            "authority host for `{}` did not become ready",
            authority_command_target_id(&command)
        )))
    }
}

fn authority_command_envelope(
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

fn should_stop(stop_rx: &mpsc::Receiver<()>) -> bool {
    stop_rx.try_recv().is_ok()
}

fn wait_or_stop(stop_rx: &mpsc::Receiver<()>, duration: Duration) -> bool {
    stop_rx.recv_timeout(duration).is_ok()
}

pub(super) fn remote_session_sync_owner_args(
    socket_name: &str,
    network: &RemoteNetworkConfig,
) -> Vec<String> {
    prepend_global_network_args(
        vec![
            "__remote-session-sync-owner".to_string(),
            "--socket-name".to_string(),
            socket_name.to_string(),
        ],
        network,
    )
}

pub(crate) fn remote_session_sync_owner_socket_path(socket_name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "waitagent-remote-session-sync-owner-{}.sock",
        sanitize_path_component(socket_name)
    ))
}

pub(crate) fn remote_session_sync_owner_available(socket_path: &Path) -> bool {
    UnixStream::connect(socket_path).is_ok()
}

pub(super) fn drain_owner_ping(listener: &UnixListener) -> io::Result<()> {
    loop {
        match listener.accept() {
            Ok((_stream, _addr)) => continue,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
            Err(error) => return Err(error),
        }
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
