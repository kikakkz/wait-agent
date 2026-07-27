use crate::cli::RemoteNetworkConfig;
use crate::domain::session_catalog::{
    ManagedSessionAddress, ManagedSessionRecord, ManagedSessionTaskState, SessionAvailability,
};
use crate::infra::error_log::ERROR_LOG;
use crate::lifecycle::LifecycleError;
use crate::runtime::ratatui_remote_connect::connect_remote_host;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// Ratatui node server: holds session state and survives TUI client disconnects.
///
/// A node server is scoped to a single session name. Multiple clients can attach
/// to the same session; the server keeps running until it is explicitly stopped.
pub struct RatatuiNodeRuntime {
    session_name: String,
    network: RemoteNetworkConfig,
    sessions: Arc<Mutex<Vec<ManagedSessionRecord>>>,
    active_target: Arc<Mutex<Option<String>>>,
}

impl RatatuiNodeRuntime {
    pub fn from_session_with_endpoints(
        session_name: String,
        listener_display: Option<String>,
        connect_endpoint: Option<String>,
    ) -> Result<Self, LifecycleError> {
        let sessions = Arc::new(Mutex::new(vec![default_local_session_record(
            &session_name,
        )]));
        let active_target = sessions
            .lock()
            .unwrap()
            .first()
            .map(|session| session.address.qualified_target());
        let network =
            ratatui_network_config(listener_display.as_deref(), connect_endpoint.as_deref());
        Ok(Self {
            session_name,
            network,
            sessions,
            active_target: Arc::new(Mutex::new(active_target)),
        })
    }

    #[allow(dead_code)]
    pub fn from_session_with_network(
        session_name: String,
        network: RemoteNetworkConfig,
    ) -> Result<Self, LifecycleError> {
        let sessions = Arc::new(Mutex::new(vec![default_local_session_record(
            &session_name,
        )]));
        let active_target = sessions
            .lock()
            .unwrap()
            .first()
            .map(|session| session.address.qualified_target());
        Ok(Self {
            session_name,
            network,
            sessions,
            active_target: Arc::new(Mutex::new(active_target)),
        })
    }

    pub fn run(&self) -> Result<(), LifecycleError> {
        let socket_path = ratatui_socket_path(&self.session_name);
        let info_path = ratatui_info_path(&self.session_name);
        ERROR_LOG.log(format!(
            "[ratatui-node] starting for session={} socket={}",
            self.session_name,
            socket_path.display()
        ));

        // Ensure the per-user socket directory exists.
        if let Some(parent) = socket_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        // Remove stale socket before binding.
        let _ = std::fs::remove_file(&socket_path);
        if let Err(error) = write_socket_info(&info_path, &self.session_name, 0) {
            ERROR_LOG.log(format!(
                "[ratatui-node] failed to write socket info: {error:?}"
            ));
        }

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

        let clients: Arc<Mutex<Vec<ClientHandle>>> = Arc::new(Mutex::new(Vec::new()));
        let client_count = Arc::new(AtomicUsize::new(0));

        let info_path = Arc::new(info_path);
        for stream in listener.incoming() {
            match stream {
                Ok(stream) => {
                    let clients = clients.clone();
                    let client_count = client_count.clone();
                    let info_path = info_path.clone();
                    let session_name = self.session_name.clone();
                    let network = self.network.clone();
                    let sessions = self.sessions.clone();
                    let active_target = self.active_target.clone();
                    std::thread::spawn(move || {
                        let client_id = NEXT_CLIENT_ID.fetch_add(1, Ordering::SeqCst);
                        if let Err(error) = handle_client(
                            stream,
                            client_id,
                            clients,
                            client_count,
                            &info_path,
                            &session_name,
                            &network,
                            sessions,
                            active_target,
                        ) {
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
        let _ = std::fs::remove_file(&*info_path);
        Ok(())
    }
}

struct ClientHandle {
    id: u64,
    stream: UnixStream,
    removed: Arc<AtomicBool>,
}

static NEXT_CLIENT_ID: AtomicU64 = AtomicU64::new(1);

fn handle_client(
    mut stream: UnixStream,
    client_id: u64,
    clients: Arc<Mutex<Vec<ClientHandle>>>,
    client_count: Arc<AtomicUsize>,
    info_path: &std::path::Path,
    session_name: &str,
    network: &RemoteNetworkConfig,
    sessions: Arc<Mutex<Vec<ManagedSessionRecord>>>,
    active_target: Arc<Mutex<Option<String>>>,
) -> Result<(), LifecycleError> {
    ERROR_LOG.log(format!("[ratatui-node] client {client_id} connected"));
    let removed = Arc::new(AtomicBool::new(false));
    client_count.fetch_add(1, Ordering::SeqCst);
    update_info_client_count(&client_count, info_path);

    if let Ok(clone) = stream.try_clone() {
        clients.lock().unwrap().push(ClientHandle {
            id: client_id,
            stream: clone,
            removed: removed.clone(),
        });
    }

    // Send snapshot so the client has something to render.
    let count = client_count.load(Ordering::SeqCst);
    let snapshot = build_snapshot(session_name, count, network, &sessions, &active_target);
    let json = serde_json::to_string(&snapshot).unwrap_or_default();
    if let Err(error) = writeln!(stream, "{json}") {
        ERROR_LOG.log(format!(
            "[ratatui-node] failed to write snapshot: {error:?}"
        ));
        remove_client(client_id, &clients, &client_count, info_path);
        return Ok(());
    }
    if let Err(error) = stream.flush() {
        ERROR_LOG.log(format!(
            "[ratatui-node] failed to flush snapshot: {error:?}"
        ));
        remove_client(client_id, &clients, &client_count, info_path);
        return Ok(());
    }

    // Read until client disconnects or sends a control command.
    let reader = stream.try_clone().map_err(|error| {
        LifecycleError::Io("failed to clone ratatui client stream".to_string(), error)
    })?;
    let mut reader = BufReader::new(reader);
    let mut line = String::new();
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
                        detach_all_clients(&clients, &client_count, info_path);
                        forcibly_detached = true;
                        break;
                    }
                    _ => {
                        if let Some(response) = handle_control_command(
                            trimmed,
                            session_name,
                            network,
                            &sessions,
                            &active_target,
                        ) {
                            let _ = writeln!(stream, "{response}");
                            let _ = stream.flush();
                            let _ = broadcast_snapshot(
                                &clients,
                                session_name,
                                network,
                                &sessions,
                                &active_target,
                            );
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
        remove_client(client_id, &clients, &client_count, info_path);
    }
    Ok(())
}

fn handle_control_command(
    command: &str,
    session_name: &str,
    network: &RemoteNetworkConfig,
    sessions: &Arc<Mutex<Vec<ManagedSessionRecord>>>,
    active_target: &Arc<Mutex<Option<String>>>,
) -> Option<String> {
    if command == "CREATE_LOCAL_SESSION" {
        let mut guard = sessions.lock().unwrap();
        let id = format!("local-{}", guard.len() + 1);
        let record = ManagedSessionRecord {
            address: ManagedSessionAddress::local_tmux(session_name, &id),
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
        guard.push(record);
        Some("OK".to_string())
    } else if let Some(target) = command.strip_prefix("ACTIVATE_TARGET ") {
        let target = target.to_string();
        let guard = sessions.lock().unwrap();
        let exists = guard
            .iter()
            .any(|session| session.address.qualified_target() == target);
        drop(guard);
        if exists {
            *active_target.lock().unwrap() = Some(target);
            Some("OK".to_string())
        } else {
            Some("ERR unknown target".to_string())
        }
    } else if let Some(profile_name) = command.strip_prefix("CONNECT_REMOTE_HOST ") {
        match connect_remote_host(profile_name, sessions, network) {
            Ok(record) => {
                let target = record.address.qualified_target();
                let mut guard = sessions.lock().unwrap();
                guard.retain(|session| session.address.id() != record.address.id());
                guard.push(record);
                drop(guard);
                *active_target.lock().unwrap() = Some(target.clone());
                Some(format!("OK connected {target}"))
            }
            Err(error) => Some(format!("ERR {error}")),
        }
    } else {
        None
    }
}

fn default_local_session_record(session_name: &str) -> ManagedSessionRecord {
    ManagedSessionRecord {
        address: ManagedSessionAddress::local_tmux(session_name, "main"),
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

fn build_snapshot(
    session_name: &str,
    client_count: usize,
    network: &RemoteNetworkConfig,
    sessions: &Arc<Mutex<Vec<ManagedSessionRecord>>>,
    active_target: &Arc<Mutex<Option<String>>>,
) -> RatatuiSnapshot {
    let records = sessions.lock().unwrap();
    let sessions: Vec<SessionView> = records.iter().map(SessionView::from_record).collect();
    let active_target = active_target.lock().unwrap().clone();

    RatatuiSnapshot {
        session_name: session_name.to_string(),
        client_count,
        main: "Main pane placeholder".to_string(),
        sidebar: "Sessions".to_string(),
        footer: FooterState {
            active_session: session_name.to_string(),
            sessions: vec![],
            listener_endpoint: Some(network.advertised_listener_label().to_string()),
            connect_endpoint: network.connect_endpoint_uri(),
            remote_count: sessions
                .iter()
                .filter(|session| session.transport == "remote")
                .count(),
        },
        sessions,
        active_target,
    }
}

fn broadcast_snapshot(
    clients: &Arc<Mutex<Vec<ClientHandle>>>,
    session_name: &str,
    network: &RemoteNetworkConfig,
    sessions: &Arc<Mutex<Vec<ManagedSessionRecord>>>,
    active_target: &Arc<Mutex<Option<String>>>,
) {
    let snapshot = build_snapshot(session_name, 0, network, sessions, active_target);
    let json = serde_json::to_string(&snapshot).unwrap_or_default();
    let mut guard = clients.lock().unwrap();
    guard.retain(|handle| !handle.removed.load(Ordering::SeqCst));
    for handle in guard.iter() {
        let mut stream = &handle.stream;
        let _ = writeln!(stream, "{json}");
        let _ = stream.flush();
    }
}

fn ratatui_network_config(
    listener_display: Option<&str>,
    connect_endpoint: Option<&str>,
) -> RemoteNetworkConfig {
    let mut network = RemoteNetworkConfig::default();
    if let Some(display) = listener_display {
        if let Some((_, port_str)) = display.rsplit_once(':') {
            if let Ok(port) = port_str.parse::<u16>() {
                network.port = port;
            }
        }
    }
    network.connect = connect_endpoint.map(String::from);
    network
}

fn detach_all_clients(
    clients: &Arc<Mutex<Vec<ClientHandle>>>,
    client_count: &Arc<AtomicUsize>,
    info_path: &std::path::Path,
) {
    ERROR_LOG.log("[ratatui-node] detaching all clients".to_string());
    let mut guard = clients.lock().unwrap();
    for handle in guard.drain(..) {
        handle.removed.store(true, Ordering::SeqCst);
        let _ = handle.stream.shutdown(std::net::Shutdown::Both);
    }
    client_count.store(0, Ordering::SeqCst);
    update_socket_info_client_count(info_path, 0);
}

fn remove_client(
    client_id: u64,
    clients: &Arc<Mutex<Vec<ClientHandle>>>,
    client_count: &Arc<AtomicUsize>,
    info_path: &std::path::Path,
) {
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
        client_count.fetch_sub(1, Ordering::SeqCst);
        update_info_client_count(client_count, info_path);
    }
}

fn update_info_client_count(client_count: &Arc<AtomicUsize>, info_path: &std::path::Path) {
    let count = client_count.load(Ordering::SeqCst);
    update_socket_info_client_count(info_path, count);
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

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RatatuiSocketInfo {
    pub session_name: String,
    pub pid: u32,
    pub client_count: usize,
    pub created_at_unix_secs: u64,
}

fn write_socket_info(
    info_path: &Path,
    session_name: &str,
    client_count: usize,
) -> Result<(), LifecycleError> {
    let info = RatatuiSocketInfo {
        session_name: session_name.to_string(),
        pid: std::process::id(),
        client_count,
        created_at_unix_secs: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    };
    write_socket_info_internal(info_path, &info)
}

fn update_socket_info_client_count(info_path: &Path, client_count: usize) {
    if let Ok(json) = std::fs::read_to_string(info_path) {
        if let Ok(mut info) = serde_json::from_str::<RatatuiSocketInfo>(&json) {
            info.client_count = client_count;
            let _ = write_socket_info_internal(info_path, &info);
        }
    }
}

fn write_socket_info_internal(
    info_path: &Path,
    info: &RatatuiSocketInfo,
) -> Result<(), LifecycleError> {
    let json = serde_json::to_string(info).map_err(|error| {
        LifecycleError::Io(
            "failed to serialize ratatui socket info".to_string(),
            error.into(),
        )
    })?;
    std::fs::write(info_path, json).map_err(|error| {
        LifecycleError::Io(
            format!(
                "failed to write ratatui socket info {}",
                info_path.display()
            ),
            error,
        )
    })
}

/// Returns the per-user directory that holds ratatui sockets.
///
/// Mirrors tmux's convention: a user-specific subdirectory under the system
/// temp directory so sessions from different users do not collide.
pub fn ratatui_socket_dir() -> PathBuf {
    std::env::temp_dir().join(format!("waitagent-ratatui-{}", effective_uid()))
}

/// Returns a stable Unix socket path for a given session.
pub fn ratatui_socket_path(session_name: &str) -> PathBuf {
    ratatui_socket_dir().join(format!("{session_name}.sock"))
}

/// Returns the companion info file path for a socket.
pub fn ratatui_info_path(session_name: &str) -> PathBuf {
    ratatui_socket_path(session_name).with_extension("sock.info")
}

fn effective_uid() -> u32 {
    unsafe { geteuid() }
}

extern "C" {
    fn geteuid() -> u32;
}

/// Returns true if a ratatui node server appears to be listening for this session.
pub fn node_is_running(session_name: &str) -> bool {
    let socket_path = ratatui_socket_path(session_name);
    if !socket_path.exists() {
        return false;
    }
    UnixStream::connect(&socket_path).is_ok()
}

/// Removes the socket and info files for a session, if any.
pub fn remove_node_socket(session_name: &str) {
    let _ = std::fs::remove_file(ratatui_socket_path(session_name));
    let _ = std::fs::remove_file(ratatui_info_path(session_name));
}

/// Reads the socket info file for a session, if it exists.
pub fn read_socket_info(session_name: &str) -> Option<RatatuiSocketInfo> {
    let info_path = ratatui_info_path(session_name);
    let json = std::fs::read_to_string(&info_path).ok()?;
    serde_json::from_str(&json).ok()
}

#[cfg(test)]
mod tests {
    use super::{ratatui_info_path, ratatui_socket_dir, ratatui_socket_path};

    #[test]
    fn socket_path_is_in_user_specific_dir() {
        let path = ratatui_socket_path("1");
        assert!(path.starts_with(ratatui_socket_dir()));
        assert!(path.as_os_str().to_string_lossy().ends_with(".sock"));
    }

    #[test]
    fn socket_path_differs_by_session() {
        let path1 = ratatui_socket_path("1");
        let path2 = ratatui_socket_path("2");
        assert_ne!(path1, path2);
    }

    #[test]
    fn info_path_is_next_to_socket() {
        let socket = ratatui_socket_path("1");
        let info = ratatui_info_path("1");
        assert_eq!(info, socket.with_extension("sock.info"));
    }
}
