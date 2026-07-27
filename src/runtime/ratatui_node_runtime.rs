use crate::infra::error_log::ERROR_LOG;
use crate::lifecycle::LifecycleError;
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
}

impl RatatuiNodeRuntime {
    pub fn from_session(session_name: String) -> Result<Self, LifecycleError> {
        Ok(Self { session_name })
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
                    std::thread::spawn(move || {
                        let client_id = NEXT_CLIENT_ID.fetch_add(1, Ordering::SeqCst);
                        if let Err(error) = handle_client(
                            stream,
                            client_id,
                            clients,
                            client_count,
                            &info_path,
                            &session_name,
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
    let snapshot = format!(
        "SNAPSHOT|{session_name}|{count}|Main pane placeholder|Sidebar placeholder|Ctrl-N New · Ctrl-W Conn · Ctrl-S Remote · Ctrl-O Hist · Ctrl-E Logs · Ctrl-M Menu"
    );
    if let Err(error) = writeln!(stream, "{snapshot}") {
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
                    _ => {}
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
