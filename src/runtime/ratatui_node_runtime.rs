use crate::cli::{RemoteNetworkConfig, RemoteRuntimeOwnerCommand};
use crate::domain::session_catalog::{
    ManagedSessionAddress, ManagedSessionRecord, ManagedSessionTaskState, SessionAvailability,
};
use crate::infra::error_log::ERROR_LOG;
use crate::lifecycle::LifecycleError;
use crate::runtime::ratatui_remote_connect::connect_remote_host;
use crate::runtime::remote_node::remote_node_ingress_server_runtime::{
    remote_node_ingress_owner_socket_path, start_owner_control_acceptor, OwnerLifecycleEvent,
    RemoteNodeIngressServerRuntime,
};
use crate::runtime::remote_node::remote_node_session_sync_runtime::{
    RatatuiLocalAuthorityHostBackend, RatatuiLocalSessionCatalog, RatatuiLocalTargetExitObserver,
    RatatuiLocalTargetFactory, RemoteNodeSessionSyncRuntime,
};
use crate::runtime::remote_publication::remote_target_publication_backend::
    RatatuiRemoteTargetPublicationBackend;
use crate::runtime::remote_publication::remote_target_publication_runtime::
    RemoteTargetPublicationRuntime;
use crate::runtime::remote_runtime_owner_runtime::RemoteRuntimeOwnerRuntime;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const DEFAULT_SESSION_ID: &str = "1";

/// Ratatui node server: holds all session state for a single `--port` and
/// survives TUI client disconnects.
///
/// One server process maps to one listener port. Multiple clients can attach to
/// the same server. Sessions inside the server are created via the TUI
/// (Ctrl+N/W/S), not via command-line arguments.
pub struct RatatuiNodeRuntime {
    network: RemoteNetworkConfig,
    shared: Arc<SharedState>,
    remote_owner: RemoteRuntimeOwnerRuntime,
}

pub(crate) struct SharedState {
    network: RemoteNetworkConfig,
    pub(crate) sessions: Mutex<HashMap<String, ManagedSessionRecord>>,
    pub(crate) active_target: Mutex<Option<String>>,
    client_count: AtomicUsize,
    clients: Arc<Mutex<Vec<ClientHandle>>>,
    start_time: Instant,
    shutdown: AtomicBool,
}

impl SharedState {
    pub(crate) fn broadcast_snapshot(&self) -> Result<(), LifecycleError> {
        broadcast_snapshot(&self.clients, self)
    }
}

impl RatatuiNodeRuntime {
    pub fn from_network(network: RemoteNetworkConfig) -> Result<Self, LifecycleError> {
        let mut sessions = HashMap::new();
        let default_record = default_local_session_record(DEFAULT_SESSION_ID);
        let active_target = Some(default_record.address.qualified_target());
        sessions.insert(DEFAULT_SESSION_ID.to_string(), default_record);

        let remote_owner = RemoteRuntimeOwnerRuntime::from_build_env_with_network(network.clone())?;

        Ok(Self {
            network: network.clone(),
            shared: Arc::new(SharedState {
                network,
                sessions: Mutex::new(sessions),
                active_target: Mutex::new(active_target),
                client_count: AtomicUsize::new(0),
                clients: Arc::new(Mutex::new(Vec::new())),
                start_time: Instant::now(),
                shutdown: AtomicBool::new(false),
            }),
            remote_owner,
        })
    }

    pub fn run(&self) -> Result<(), LifecycleError> {
        let socket_path = ratatui_socket_path(self.network.port);
        ERROR_LOG.log(format!(
            "[ratatui-node] starting port={} socket={}",
            self.network.port,
            socket_path.display()
        ));

        if let Some(parent) = socket_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        // Remove stale socket before binding.
        let _ = std::fs::remove_file(&socket_path);

        let listener = UnixListener::bind(&socket_path).map_err(|error| {
            LifecycleError::Io(
                format!(
                    "failed to bind ratatui node socket {}",
                    socket_path.display()
                ),
                error,
            )
        })?;

        ERROR_LOG.log("[ratatui-node] listening".to_string());

        // Start the remote runtime owner inside the server process so that
        // discovered remote sessions are kept in-memory and shared with the
        // remote target publication / sync runtimes.
        let owner_network = self.network.clone();
        let owner = self.remote_owner.clone();
        let _owner_thread = std::thread::spawn(move || {
            if let Err(error) = owner.run_owner(RemoteRuntimeOwnerCommand::default()) {
                ERROR_LOG.log(format!(
                    "[ratatui-node] remote runtime owner exited: {error}"
                ));
            }
        });

        // Start the remote session sync runtime when a connect endpoint is
        // configured. It publishes the local ratatui session catalog to the
        // remote authority and accepts create-session requests from it.
        let _sync_guard = if self.network.connect.is_some() {
            let sync_network = self.network.clone();
            let shared = self.shared.clone();
            let backend = RatatuiRemoteTargetPublicationBackend::new(shared.clone(), sync_network.clone());
            let publication_runtime =
                RemoteTargetPublicationRuntime::with_network_and_backend(sync_network.clone(), backend)?;
            let sync_runtime = RemoteNodeSessionSyncRuntime::new_with_backends(
                RatatuiLocalSessionCatalog::new(shared.clone()),
                crate::infra::remote_grpc_transport::GrpcRemoteNodeTransport::new(),
                RatatuiLocalTargetExitObserver,
                RatatuiLocalTargetFactory::new(shared.clone(), sync_network.clone()),
                RatatuiLocalAuthorityHostBackend::new(shared.clone(), sync_network.clone()),
                Some(publication_runtime),
                sync_network,
            );
            let (_catalog_tx, catalog_rx) = std::sync::mpsc::channel();
            match sync_runtime.start_with_local_catalog_changes(catalog_rx) {
                Ok(guard) => Some(guard),
                Err(error) => {
                    ERROR_LOG.log(format!(
                        "[ratatui-node] failed to start remote session sync: {error}"
                    ));
                    None
                }
            }
        } else {
            None
        };

        // Start the remote node ingress server inside the server process so
        // peers can connect in and request local target sessions.
        let ingress_network = self.network.clone();
        let shared = self.shared.clone();
        let ingress_backend = RatatuiRemoteTargetPublicationBackend::new(shared.clone(), ingress_network.clone());
        let ingress_publication_runtime =
            RemoteTargetPublicationRuntime::with_network_and_backend(ingress_network.clone(), ingress_backend)?;
        let ingress_runtime = RemoteNodeIngressServerRuntime::new_with_backends(
            ingress_network,
            ingress_publication_runtime,
            RatatuiLocalTargetFactory::new(shared.clone(), self.network.clone()),
            RatatuiLocalAuthorityHostBackend::new(shared.clone(), self.network.clone()),
        );
        let _ingress_guard = match ingress_runtime.start() {
            Ok(guard) => {
                // Bind the same local control socket that tmux-sidecar ingress
                // owners use, so __remote-session-creation and Ctrl+W connect
                // can reach this single-process server.
                let owner_socket_path = remote_node_ingress_owner_socket_path(&self.network);
                let _ = std::fs::remove_file(&owner_socket_path);
                if let Ok(owner_listener) = std::os::unix::net::UnixListener::bind(&owner_socket_path)
                {
                    if let Some(owner_tx) = guard.owner_event_sender() {
                        let (_lifecycle_tx, _lifecycle_rx) = std::sync::mpsc::channel::<OwnerLifecycleEvent>();
                        let _owner_acceptor =
                            start_owner_control_acceptor(owner_listener, &owner_tx, _lifecycle_tx);
                    }
                }
                Some(guard)
            }
            Err(error) => {
                ERROR_LOG.log(format!(
                    "[ratatui-node] failed to start remote node ingress: {error}"
                ));
                None
            }
        };

        let clients = self.shared.clients.clone();

        for stream in listener.incoming() {
            if self.shared.shutdown.load(Ordering::SeqCst) {
                break;
            }

            match stream {
                Ok(stream) => {
                    let clients = clients.clone();
                    let shared = self.shared.clone();
                    std::thread::spawn(move || {
                        let client_id = NEXT_CLIENT_ID.fetch_add(1, Ordering::SeqCst);
                        if let Err(error) = handle_client(stream, client_id, clients, shared) {
                            ERROR_LOG
                                .log(format!("[ratatui-node] client handler error: {error:?}"));
                        }
                    });
                }
                Err(error) => {
                    ERROR_LOG.log(format!("[ratatui-node] accept error: {error:?}"));
                }
            }
        }

        let _ = std::fs::remove_file(&socket_path);
        if let Err(error) = RemoteRuntimeOwnerRuntime::shutdown_owner(&owner_network) {
            ERROR_LOG.log(format!(
                "[ratatui-node] remote runtime owner shutdown error: {error}"
            ));
        }
        ERROR_LOG.log(format!(
            "[ratatui-node] shutting down port={}",
            self.network.port
        ));
        Ok(())
    }
}

pub(crate) struct ClientHandle {
    id: u64,
    stream: UnixStream,
    removed: Arc<AtomicBool>,
}

static NEXT_CLIENT_ID: AtomicU64 = AtomicU64::new(1);

fn handle_client(
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
            .unwrap_or_else(|| "ERR unknown command".to_string());
        let _ = writeln!(stream, "{response}");
        let _ = stream.flush();
        return Ok(());
    }

    // Register as a TUI client and send the initial snapshot.
    shared.client_count.fetch_add(1, Ordering::SeqCst);
    if let Ok(clone) = stream.try_clone() {
        clients.lock().unwrap().push(ClientHandle {
            id: client_id,
            stream: clone,
            removed: removed.clone(),
        });
    }

    // The "ATTACH" command is a no-op beyond triggering the snapshot.
    let count = shared.client_count.load(Ordering::SeqCst);
    let snapshot = build_snapshot(count, &shared);
    let json = serde_json::to_string(&snapshot).unwrap_or_default();
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
                        detach_all_clients(&clients, &shared);
                        forcibly_detached = true;
                        break;
                    }
                    _ => {
                        if let Some(response) =
                            handle_control_command(trimmed, &shared, &mut stream)
                        {
                            let _ = writeln!(stream, "{response}");
                            let _ = stream.flush();
                            if response_should_broadcast(&response) {
                                let _ = broadcast_snapshot(&clients, &shared);
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

fn is_one_shot_control_command(command: &str) -> bool {
    matches!(command, "STATUS" | "STOP" | "LIST_SESSIONS" | "DETACH_ALL")
        || command.starts_with("CONNECT_REMOTE_HOST ")
}

fn response_should_broadcast(response: &str) -> bool {
    response.starts_with("OK")
}

fn handle_control_command(
    command: &str,
    shared: &Arc<SharedState>,
    _stream: &mut UnixStream,
) -> Option<String> {
    if command == "STATUS" {
        let count = shared.client_count.load(Ordering::SeqCst);
        let uptime = shared.start_time.elapsed().as_secs();
        let session_count = shared.sessions.lock().unwrap().len();
        let status = ServerStatus {
            port: shared.network.port,
            client_count: count,
            uptime_secs: uptime,
            session_count,
        };
        return Some(serde_json::to_string(&status).unwrap_or_default());
    }

    if command == "STOP" {
        shared.shutdown.store(true, Ordering::SeqCst);
        // Wake up the listener by connecting to it from this side.
        let _ = UnixStream::connect(ratatui_socket_path(shared.network.port));
        return Some("OK stopping".to_string());
    }

    if command == "LIST_SESSIONS" {
        let guard = shared.sessions.lock().unwrap();
        let sessions: Vec<SessionView> = guard.values().map(SessionView::from_record).collect();
        drop(guard);
        return Some(serde_json::to_string(&sessions).unwrap_or_default());
    }

    if command == "CREATE_LOCAL_SESSION" {
        let mut guard = shared.sessions.lock().unwrap();
        let id = format!("{}", guard.len() + 1);
        let record = ManagedSessionRecord {
            address: ManagedSessionAddress::local_tmux(&id, "main"),
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
            task_state: ManagedSessionTaskState::Running,
        };
        let target = record.address.qualified_target();
        guard.insert(id, record);
        drop(guard);
        *shared.active_target.lock().unwrap() = Some(target);
        return Some("OK created local session".to_string());
    }

    if let Some(target) = command.strip_prefix("ACTIVATE_TARGET ") {
        let target = target.to_string();
        let guard = shared.sessions.lock().unwrap();
        let exists = guard
            .values()
            .any(|session| session.address.qualified_target() == target);
        drop(guard);
        if exists {
            *shared.active_target.lock().unwrap() = Some(target);
            return Some("OK".to_string());
        }
        return Some("ERR unknown target".to_string());
    }

    if let Some(profile_name) = command.strip_prefix("CONNECT_REMOTE_HOST ") {
        // Build a temporary Vec view for the existing connect helper.
        let sessions_vec: Vec<ManagedSessionRecord> = {
            let guard = shared.sessions.lock().unwrap();
            guard.values().cloned().collect()
        };
        let sessions_arc = Arc::new(Mutex::new(sessions_vec));
        match connect_remote_host(profile_name, &sessions_arc, &shared.network) {
            Ok(record) => {
                let target = record.address.qualified_target();
                let mut guard = shared.sessions.lock().unwrap();
                guard.retain(|_, session| session.address.id() != record.address.id());
                guard.insert(record.address.session_id().to_string(), record);
                drop(guard);
                *shared.active_target.lock().unwrap() = Some(target.clone());
                Some(format!("OK connected {target}"))
            }
            Err(error) => Some(format!("ERR {error}")),
        }
    } else {
        None
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ServerStatus {
    pub port: u16,
    pub client_count: usize,
    pub uptime_secs: u64,
    pub session_count: usize,
}

fn default_local_session_record(session_id: &str) -> ManagedSessionRecord {
    ManagedSessionRecord {
        address: ManagedSessionAddress::local_tmux(session_id, "main"),
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
    }
}

fn build_snapshot(client_count: usize, shared: &SharedState) -> RatatuiSnapshot {
    let guard = shared.sessions.lock().unwrap();
    let sessions: Vec<SessionView> = guard.values().map(SessionView::from_record).collect();
    let active_target = shared.active_target.lock().unwrap().clone();
    let active_session_id = active_target
        .as_deref()
        .and_then(|target| {
            guard
                .values()
                .find(|s| s.address.qualified_target() == target)
        })
        .map(|s| s.address.session_id().to_string())
        .unwrap_or_else(|| DEFAULT_SESSION_ID.to_string());
    drop(guard);

    RatatuiSnapshot {
        session_name: active_session_id.clone(),
        client_count,
        main: "Main pane placeholder".to_string(),
        sidebar: "Sessions".to_string(),
        footer: FooterState {
            active_session: active_session_id,
            sessions: vec![],
            listener_endpoint: Some(shared.network.advertised_listener_label().to_string()),
            connect_endpoint: shared.network.connect_endpoint_uri(),
            remote_count: sessions
                .iter()
                .filter(|session| session.transport == "remote")
                .count(),
        },
        sessions,
        active_target,
    }
}

pub(crate) fn broadcast_snapshot(
    clients: &Arc<Mutex<Vec<ClientHandle>>>,
    shared: &SharedState,
) -> Result<(), LifecycleError> {
    let snapshot = build_snapshot(0, shared);
    let json = serde_json::to_string(&snapshot).unwrap_or_default();
    let mut guard = clients.lock().unwrap();
    guard.retain(|handle| !handle.removed.load(Ordering::SeqCst));
    for handle in guard.iter() {
        let mut stream = &handle.stream;
        let _ = writeln!(stream, "{json}");
        let _ = stream.flush();
    }
    Ok(())
}

fn detach_all_clients(clients: &Arc<Mutex<Vec<ClientHandle>>>, shared: &SharedState) {
    ERROR_LOG.log("[ratatui-node] detaching all clients".to_string());
    let mut guard = clients.lock().unwrap();
    for handle in guard.drain(..) {
        handle.removed.store(true, Ordering::SeqCst);
        let _ = handle.stream.shutdown(std::net::Shutdown::Both);
    }
    shared.client_count.store(0, Ordering::SeqCst);
}

fn remove_client(client_id: u64, clients: &Arc<Mutex<Vec<ClientHandle>>>, shared: &SharedState) {
    let already_removed = {
        let mut guard = clients.lock().unwrap();
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

/// Snapshot sent from the node server to a TUI client on attach and update.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct RatatuiSnapshot {
    pub session_name: String,
    pub client_count: usize,
    pub main: String,
    pub sidebar: String,
    pub footer: FooterState,
    pub sessions: Vec<SessionView>,
    pub active_target: Option<String>,
}

/// Serializable session row exposed to the TUI client for sidebar rendering.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SessionView {
    pub id: String,
    pub transport: String,
    pub command_name: String,
    pub authority_node_id: String,
    pub display_authority_id: String,
    pub session_id: String,
    pub task_state: String,
    pub availability: String,
    pub attached_clients: usize,
}

impl SessionView {
    fn from_record(record: &ManagedSessionRecord) -> Self {
        let command_name = record
            .display_command_name
            .as_deref()
            .or(record.command_name.as_deref())
            .unwrap_or("bash")
            .to_string();
        let authority_node_id = record.address.authority_id().to_string();
        let display_authority_id = record.address.display_authority_id().to_string();
        Self {
            id: record.address.qualified_target(),
            transport: match record.address.transport() {
                crate::domain::session_catalog::SessionTransport::LocalTmux => "local".to_string(),
                crate::domain::session_catalog::SessionTransport::RemotePeer => {
                    "remote".to_string()
                }
            },
            command_name,
            authority_node_id,
            display_authority_id,
            session_id: record.address.session_id().to_string(),
            task_state: record.task_state.as_str().to_string(),
            availability: record.availability.as_str().to_string(),
            attached_clients: record.attached_clients,
        }
    }

    pub fn display_label(&self) -> String {
        match self.transport.as_str() {
            "local" => format!("{}@local", self.command_name),
            _ => self.remote_row_label(),
        }
    }

    fn remote_row_label(&self) -> String {
        let (host, port) = self
            .authority_node_id
            .split_once('#')
            .map(|(host, port)| (host, Some(port)))
            .unwrap_or((self.display_authority_id.as_str(), None));
        match port {
            Some(port) => format!("{}@{}:{}", self.command_name, host, port),
            None => format!("{}@{}", self.command_name, host),
        }
    }

    pub fn display_label_candidates(&self) -> Vec<String> {
        match self.transport.as_str() {
            "local" => vec![self.display_label()],
            _ => {
                let mut candidates = Vec::new();
                let full = self.remote_row_label();
                let host_only = format!("{}@{}", self.command_name, self.display_authority_id);
                if full != host_only {
                    candidates.push(full);
                }
                candidates.push(host_only);
                candidates
            }
        }
    }
}

/// Footer state rendered by the TUI client.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct FooterState {
    pub active_session: String,
    pub sessions: Vec<SessionSummary>,
    pub listener_endpoint: Option<String>,
    pub connect_endpoint: Option<String>,
    pub remote_count: usize,
}

/// A single entry in the footer session list.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct SessionSummary {
    pub name: String,
    pub client_count: usize,
}

/// Returns the per-user directory that holds ratatui sockets.
///
/// Mirrors tmux's convention: a user-specific subdirectory under the system
/// temp directory so sockets from different users do not collide.
pub fn ratatui_socket_dir() -> PathBuf {
    std::env::temp_dir().join(format!("waitagent-ratatui-{}", effective_uid()))
}

/// Returns a stable Unix socket path for a given listener port.
pub fn ratatui_socket_path(port: u16) -> PathBuf {
    ratatui_socket_dir().join(format!("{port}.sock"))
}

fn effective_uid() -> u32 {
    unsafe { geteuid() }
}

extern "C" {
    fn geteuid() -> u32;
}

/// Returns true if a ratatui node server appears to be listening on the port.
pub fn node_is_running(port: u16) -> bool {
    let socket_path = ratatui_socket_path(port);
    if !socket_path.exists() {
        return false;
    }
    UnixStream::connect(&socket_path).is_ok()
}

/// Removes the socket file for a port, if any.
pub fn remove_node_socket(port: u16) {
    let _ = std::fs::remove_file(ratatui_socket_path(port));
}

/// Sends a one-shot control command to the server on `port` and returns the
/// first line of response. Used by `ls`, `detach`, `stop`, `list-sessions`.
pub fn send_node_command(port: u16, command: &str) -> Result<String, LifecycleError> {
    let socket_path = ratatui_socket_path(port);
    let mut stream = UnixStream::connect(&socket_path).map_err(|error| {
        LifecycleError::Io(
            format!(
                "failed to connect to ratatui node socket {}",
                socket_path.display()
            ),
            error,
        )
    })?;
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .map_err(|error| {
            LifecycleError::Io(
                "failed to set read timeout on ratatui node socket".to_string(),
                error,
            )
        })?;
    writeln!(stream, "{command}").map_err(|error| {
        LifecycleError::Io("failed to send command to ratatui node".to_string(), error)
    })?;
    stream.flush().map_err(|error| {
        LifecycleError::Io("failed to flush command to ratatui node".to_string(), error)
    })?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).map_err(|error| {
        LifecycleError::Io("failed to read ratatui node response".to_string(), error)
    })?;
    Ok(line.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::{ratatui_socket_dir, ratatui_socket_path};

    #[test]
    fn socket_path_is_in_user_specific_dir() {
        let path = ratatui_socket_path(7575);
        assert!(path.starts_with(ratatui_socket_dir()));
        assert!(path.as_os_str().to_string_lossy().ends_with(".sock"));
    }

    #[test]
    fn socket_path_differs_by_port() {
        let path1 = ratatui_socket_path(7575);
        let path2 = ratatui_socket_path(7576);
        assert_ne!(path1, path2);
    }
}
