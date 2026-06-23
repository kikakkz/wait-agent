use crate::cli::{prepend_global_network_args, RemoteNetworkConfig, RemoteRuntimeOwnerCommand};
use crate::domain::session_catalog::{
    ManagedSessionAddress, ManagedSessionRecord, ManagedSessionTaskState, SessionAvailability,
};
use crate::domain::workspace::WorkspaceSessionRole;
use crate::infra::error_log::ERROR_LOG;
use crate::infra::tmux::EmbeddedTmuxBackend;
use crate::lifecycle::LifecycleError;
use crate::runtime::current_executable::current_waitagent_executable;
use crate::runtime::sidecar_process_runtime::spawn_waitagent_sidecar;
use std::collections::HashMap;
use std::fs;
use std::io::{self, BufRead, BufReader, ErrorKind, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const REMOTE_RUNTIME_OWNER_READY_RETRIES: usize = 100;
const REMOTE_RUNTIME_OWNER_READY_SLEEP: Duration = Duration::from_millis(25);
const REMOTE_RUNTIME_OWNER_LIVENESS_CHECK_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteRuntimeOwnerRuntime {
    current_executable: PathBuf,
    network: RemoteNetworkConfig,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteRuntimeOwnerSnapshot {
    pub sessions: Vec<ManagedSessionRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RemoteRuntimeOwnerCommandEnvelope {
    UpsertSession {
        node_id: String,
        session: ManagedSessionRecord,
    },
    RemoveSession {
        node_id: String,
        authority_id: String,
        transport_session_id: String,
    },
    RemoveNode {
        node_id: String,
    },
    MarkNodeOffline {
        node_id: String,
    },
    Snapshot,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OwnerStateRecord {
    node_id: String,
    session: ManagedSessionRecord,
}

#[derive(Clone)]
struct RemoteRuntimeOwnerSharedState {
    records: Arc<Mutex<HashMap<String, OwnerStateRecord>>>,
    running: Arc<AtomicBool>,
}

impl RemoteRuntimeOwnerRuntime {
    pub fn from_build_env_with_network(
        network: RemoteNetworkConfig,
    ) -> Result<Self, LifecycleError> {
        Ok(Self {
            current_executable: current_waitagent_executable()?,
            network,
        })
    }

    #[cfg(test)]
    pub fn new_for_tests(current_executable: PathBuf, network: RemoteNetworkConfig) -> Self {
        start_remote_runtime_owner_for_tests(&network);
        Self {
            current_executable,
            network,
        }
    }

    #[cfg(test)]
    pub fn network_config_for_tests(&self) -> RemoteNetworkConfig {
        self.network.clone()
    }

    pub fn run_owner(&self, _command: RemoteRuntimeOwnerCommand) -> Result<(), LifecycleError> {
        let socket_path = remote_runtime_owner_socket_path(&self.network);
        if socket_path.exists() {
            let _ = fs::remove_file(&socket_path);
        }
        let listener = UnixListener::bind(&socket_path).map_err(remote_runtime_owner_error)?;
        let state = RemoteRuntimeOwnerSharedState {
            records: Arc::new(Mutex::new(HashMap::new())),
            running: Arc::new(AtomicBool::new(true)),
        };
        let watcher_running = Arc::clone(&state.running);
        let watcher_socket_path = socket_path.clone();
        let liveness_watcher = thread::spawn(move || {
            while watcher_running.load(Ordering::Relaxed) {
                thread::sleep(REMOTE_RUNTIME_OWNER_LIVENESS_CHECK_INTERVAL);
                if any_backend_socket_still_live() {
                    continue;
                }
                watcher_running.store(false, Ordering::Relaxed);
                let _ = UnixStream::connect(&watcher_socket_path);
                break;
            }
        });
        while state.running.load(Ordering::Relaxed) {
            let accepted = match listener.accept() {
                Ok((stream, _)) => Ok(stream),
                Err(error) => Err(error),
            };
            if !state.running.load(Ordering::Relaxed) {
                break;
            }
            let Ok(mut stream) = accepted.map_err(remote_runtime_owner_error) else {
                break;
            };
            let t_client = Instant::now();
            ERROR_LOG.log("[diag-newhost] remote_owner server accepted".to_string());
            let response = handle_remote_runtime_owner_client(&state, &mut stream);
            match response {
                Ok(Some(payload)) => {
                    let t_write = Instant::now();
                    let write_ok = stream.write_all(payload.as_bytes()).is_ok();
                    let flush_ok = stream.flush().is_ok();
                    ERROR_LOG.log(format!(
                        "[diag-newhost] remote_owner server write_response bytes={} write_ok={} flush_ok={} elapsed={:?} total={:?}",
                        payload.len(),
                        write_ok,
                        flush_ok,
                        t_write.elapsed(),
                        t_client.elapsed()
                    ));
                }
                Ok(None) => {
                    ERROR_LOG.log(format!(
                        "[diag-newhost] remote_owner server no_response total={:?}",
                        t_client.elapsed()
                    ));
                }
                Err(error) => {
                    ERROR_LOG.log(format!(
                        "[diag-newhost] remote_owner server handle_error error={} total={:?}",
                        error,
                        t_client.elapsed()
                    ));
                }
            }
        }
        state.running.store(false, Ordering::Relaxed);
        let _ = UnixStream::connect(&socket_path);
        let _ = liveness_watcher.join();
        let _ = fs::remove_file(&socket_path);
        Ok(())
    }

    pub fn ensure_owner_running(&self) -> Result<(), LifecycleError> {
        ensure_remote_runtime_owner_process_running(&self.current_executable, &self.network)
    }

    pub fn upsert_session(
        &self,
        node_id: &str,
        session: &ManagedSessionRecord,
    ) -> Result<(), LifecycleError> {
        self.ensure_owner_running()?;
        signal_remote_runtime_owner_command(
            &self.current_executable,
            &self.network,
            RemoteRuntimeOwnerCommandEnvelope::UpsertSession {
                node_id: node_id.to_string(),
                session: session.clone(),
            },
        )
    }

    pub fn remove_session(
        &self,
        node_id: &str,
        authority_id: &str,
        transport_session_id: &str,
    ) -> Result<(), LifecycleError> {
        self.ensure_owner_running()?;
        signal_remote_runtime_owner_command(
            &self.current_executable,
            &self.network,
            RemoteRuntimeOwnerCommandEnvelope::RemoveSession {
                node_id: node_id.to_string(),
                authority_id: authority_id.to_string(),
                transport_session_id: transport_session_id.to_string(),
            },
        )
    }

    #[allow(dead_code)]
    pub fn remove_node(&self, node_id: &str) -> Result<(), LifecycleError> {
        self.ensure_owner_running()?;
        signal_remote_runtime_owner_command(
            &self.current_executable,
            &self.network,
            RemoteRuntimeOwnerCommandEnvelope::RemoveNode {
                node_id: node_id.to_string(),
            },
        )
    }

    pub fn mark_node_offline(&self, node_id: &str) -> Result<(), LifecycleError> {
        self.ensure_owner_running()?;
        signal_remote_runtime_owner_command(
            &self.current_executable,
            &self.network,
            RemoteRuntimeOwnerCommandEnvelope::MarkNodeOffline {
                node_id: node_id.to_string(),
            },
        )
    }

    #[cfg(test)]
    pub fn snapshot(&self) -> Result<RemoteRuntimeOwnerSnapshot, LifecycleError> {
        self.ensure_owner_running()?;
        let mut stream = UnixStream::connect(remote_runtime_owner_socket_path(&self.network))
            .map_err(remote_runtime_owner_error)?;
        stream
            .write_all(
                render_remote_runtime_owner_command(&RemoteRuntimeOwnerCommandEnvelope::Snapshot)
                    .as_bytes(),
            )
            .map_err(remote_runtime_owner_error)?;
        stream.flush().map_err(remote_runtime_owner_error)?;
        stream
            .shutdown(Shutdown::Write)
            .map_err(remote_runtime_owner_error)?;
        let mut response = String::new();
        stream
            .read_to_string(&mut response)
            .map_err(remote_runtime_owner_error)?;
        parse_remote_runtime_owner_snapshot(&response)
    }

    pub fn try_snapshot(&self) -> Result<RemoteRuntimeOwnerSnapshot, LifecycleError> {
        let t_total = Instant::now();
        let socket_path = remote_runtime_owner_socket_path(&self.network);
        if !socket_path.exists() {
            return Ok(RemoteRuntimeOwnerSnapshot {
                sessions: Vec::new(),
            });
        }
        let t_connect = Instant::now();
        let mut stream = match UnixStream::connect(&socket_path) {
            Ok(stream) => {
                ERROR_LOG.log(format!(
                    "[diag-newhost] remote_owner try_snapshot connect listener={} elapsed={:?} total={:?}",
                    self.network.listener_addr(),
                    t_connect.elapsed(),
                    t_total.elapsed()
                ));
                stream
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {
                return Ok(RemoteRuntimeOwnerSnapshot {
                    sessions: Vec::new(),
                })
            }
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::ConnectionRefused
                        | ErrorKind::ConnectionReset
                        | ErrorKind::ConnectionAborted
                        | ErrorKind::BrokenPipe
                ) =>
            {
                let _ = fs::remove_file(&socket_path);
                return Ok(RemoteRuntimeOwnerSnapshot {
                    sessions: Vec::new(),
                });
            }
            Err(error) => return Err(remote_runtime_owner_error(error)),
        };
        // Prevent indefinite blocking if the remote runtime owner process
        // is stuck (e.g., blocked on a gRPC or tmux operation after a
        // network interruption). Without this, every caller in the local
        // session switch path — sidebar refresh, __activate-target, etc. —
        // freezes when the owner cannot respond.
        let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
        // Gracefully degrade: if the owner is unresponsive, return an empty
        // snapshot so local session switching can continue without remote
        // sessions visible.
        let t_write = Instant::now();
        let write_ok = stream
            .write_all(
                render_remote_runtime_owner_command(&RemoteRuntimeOwnerCommandEnvelope::Snapshot)
                    .as_bytes(),
            )
            .is_ok();
        let flush_ok = stream.flush().is_ok();
        let shutdown_ok = stream.shutdown(Shutdown::Write).is_ok();
        ERROR_LOG.log(format!(
            "[diag-newhost] remote_owner try_snapshot write listener={} write_ok={} flush_ok={} shutdown_ok={} elapsed={:?} total={:?}",
            self.network.listener_addr(),
            write_ok,
            flush_ok,
            shutdown_ok,
            t_write.elapsed(),
            t_total.elapsed()
        ));
        let mut response = String::new();
        let t_read = Instant::now();
        match stream.read_to_string(&mut response) {
            Ok(_) => {
                ERROR_LOG.log(format!(
                    "[diag-newhost] remote_owner try_snapshot read listener={} bytes={} elapsed={:?} total={:?}",
                    self.network.listener_addr(),
                    response.len(),
                    t_read.elapsed(),
                    t_total.elapsed()
                ));
                let t_parse = Instant::now();
                match parse_remote_runtime_owner_snapshot(&response) {
                    Ok(snapshot) => {
                        ERROR_LOG.log(format!(
                            "[diag-newhost] remote_owner try_snapshot parse listener={} sessions={} elapsed={:?} total={:?}",
                            self.network.listener_addr(),
                            snapshot.sessions.len(),
                            t_parse.elapsed(),
                            t_total.elapsed()
                        ));
                        Ok(snapshot)
                    }
                    Err(_) => {
                        ERROR_LOG.log(format!(
                            "[diag-newhost] remote_owner try_snapshot parse_failed listener={} elapsed={:?} total={:?}",
                            self.network.listener_addr(),
                            t_parse.elapsed(),
                            t_total.elapsed()
                        ));
                        let _ = fs::remove_file(&socket_path);
                        Ok(RemoteRuntimeOwnerSnapshot {
                            sessions: Vec::new(),
                        })
                    }
                }
            }
            Err(error) => {
                ERROR_LOG.log(format!(
                    "[diag-newhost] remote_owner try_snapshot read_failed listener={} error={} elapsed={:?} total={:?}",
                    self.network.listener_addr(),
                    error,
                    t_read.elapsed(),
                    t_total.elapsed()
                ));
                let _ = fs::remove_file(&socket_path);
                Ok(RemoteRuntimeOwnerSnapshot {
                    sessions: Vec::new(),
                })
            }
        }
    }
}

#[cfg(test)]
fn start_remote_runtime_owner_for_tests(network: &RemoteNetworkConfig) {
    let socket_path = remote_runtime_owner_socket_path(network);
    let _ = fs::remove_file(&socket_path);
    let listener =
        UnixListener::bind(&socket_path).expect("test remote runtime owner socket should bind");
    let state = RemoteRuntimeOwnerSharedState {
        records: Arc::new(Mutex::new(HashMap::new())),
        running: Arc::new(AtomicBool::new(true)),
    };
    thread::spawn(move || {
        for accepted in listener.incoming() {
            let Ok(mut stream) = accepted else {
                break;
            };
            let response = handle_remote_runtime_owner_client(&state, &mut stream);
            if let Ok(Some(payload)) = response {
                let _ = stream.write_all(payload.as_bytes());
                let _ = stream.flush();
            }
        }
    });
}

fn handle_remote_runtime_owner_client(
    state: &RemoteRuntimeOwnerSharedState,
    stream: &mut UnixStream,
) -> Result<Option<String>, LifecycleError> {
    let t_total = Instant::now();
    let command = read_remote_runtime_owner_command(stream)?;
    let command_label = remote_runtime_owner_command_label(&command);
    ERROR_LOG.log(format!(
        "[diag-newhost] remote_owner server read_command command={} elapsed={:?}",
        command_label,
        t_total.elapsed()
    ));
    let t_handle = Instant::now();
    let response = match command {
        RemoteRuntimeOwnerCommandEnvelope::UpsertSession { node_id, session } => {
            let key = owned_record_key(&node_id, session.address.id().as_str());
            state
                .records
                .lock()
                .expect("remote runtime owner state mutex should not be poisoned")
                .insert(key, OwnerStateRecord { node_id, session });
            Ok(Some("ok\n".to_string()))
        }
        RemoteRuntimeOwnerCommandEnvelope::RemoveSession {
            node_id,
            authority_id,
            transport_session_id,
        } => {
            let target_id = ManagedSessionAddress::remote_peer(authority_id, transport_session_id)
                .id()
                .as_str()
                .to_string();
            let key = owned_record_key(&node_id, &target_id);
            state
                .records
                .lock()
                .expect("remote runtime owner state mutex should not be poisoned")
                .remove(&key);
            Ok(Some("ok\n".to_string()))
        }
        RemoteRuntimeOwnerCommandEnvelope::RemoveNode { node_id } => {
            let mut guard = state
                .records
                .lock()
                .expect("remote runtime owner state mutex should not be poisoned");
            guard.retain(|_, record| record.node_id != node_id);
            Ok(Some("ok\n".to_string()))
        }
        RemoteRuntimeOwnerCommandEnvelope::MarkNodeOffline { node_id } => {
            let mut guard = state
                .records
                .lock()
                .expect("remote runtime owner state mutex should not be poisoned");
            for record in guard.values_mut() {
                if record.node_id == node_id {
                    record.session.availability = SessionAvailability::Offline;
                }
            }
            Ok(Some("ok\n".to_string()))
        }
        RemoteRuntimeOwnerCommandEnvelope::Snapshot => {
            let snapshot = render_remote_runtime_owner_snapshot(
                &state
                    .records
                    .lock()
                    .expect("remote runtime owner state mutex should not be poisoned")
                    .values()
                    .cloned()
                    .collect::<Vec<_>>(),
            );
            Ok(Some(snapshot))
        }
    };
    ERROR_LOG.log(format!(
        "[diag-newhost] remote_owner server handled command={} ok={} elapsed={:?} total={:?}",
        command_label,
        response.is_ok(),
        t_handle.elapsed(),
        t_total.elapsed()
    ));
    response
}

fn remote_runtime_owner_command_label(command: &RemoteRuntimeOwnerCommandEnvelope) -> &'static str {
    match command {
        RemoteRuntimeOwnerCommandEnvelope::UpsertSession { .. } => "upsert_session",
        RemoteRuntimeOwnerCommandEnvelope::RemoveSession { .. } => "remove_session",
        RemoteRuntimeOwnerCommandEnvelope::RemoveNode { .. } => "remove_node",
        RemoteRuntimeOwnerCommandEnvelope::MarkNodeOffline { .. } => "mark_node_offline",
        RemoteRuntimeOwnerCommandEnvelope::Snapshot => "snapshot",
    }
}

pub(crate) fn ensure_remote_runtime_owner_process_running(
    current_executable: &Path,
    network: &RemoteNetworkConfig,
) -> Result<(), LifecycleError> {
    let socket_path = remote_runtime_owner_socket_path(network);
    if remote_runtime_owner_available(&socket_path) {
        return Ok(());
    }
    if socket_path.exists() {
        let _ = fs::remove_file(&socket_path);
    }

    spawn_waitagent_sidecar(current_executable, remote_runtime_owner_args(network))
        .map_err(remote_runtime_owner_error)?;

    for _ in 0..REMOTE_RUNTIME_OWNER_READY_RETRIES {
        if remote_runtime_owner_available(&socket_path) {
            return Ok(());
        }
        thread::sleep(REMOTE_RUNTIME_OWNER_READY_SLEEP);
    }

    Err(LifecycleError::Protocol(format!(
        "remote runtime owner for listener `{}` did not become ready at {}",
        network.listener_addr(),
        socket_path.display()
    )))
}

fn remote_runtime_owner_available(socket_path: &Path) -> bool {
    let Ok(mut stream) = UnixStream::connect(socket_path) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
    if stream
        .write_all(
            render_remote_runtime_owner_command(&RemoteRuntimeOwnerCommandEnvelope::Snapshot)
                .as_bytes(),
        )
        .is_err()
    {
        return false;
    }
    if stream.flush().is_err() || stream.shutdown(Shutdown::Write).is_err() {
        return false;
    }
    let mut response = String::new();
    stream.read_to_string(&mut response).is_ok()
        && parse_remote_runtime_owner_snapshot(&response).is_ok()
}

fn any_backend_socket_still_live() -> bool {
    let Ok(backend) = EmbeddedTmuxBackend::from_build_env() else {
        return false;
    };
    let Ok(sockets) = backend.discover_waitagent_sockets() else {
        return false;
    };
    for socket in &sockets {
        if backend.socket_is_live(socket) {
            return true;
        }
    }
    false
}

pub(crate) fn remote_runtime_owner_socket_path(network: &RemoteNetworkConfig) -> PathBuf {
    std::env::temp_dir().join(format!(
        "waitagent-remote-runtime-owner-{}.sock",
        sanitize_path_component(&network.listener_addr().to_string())
    ))
}

pub(crate) fn remote_runtime_owner_args(network: &RemoteNetworkConfig) -> Vec<String> {
    prepend_global_network_args(vec!["__remote-runtime-owner".to_string()], network)
}

fn signal_remote_runtime_owner_command(
    current_executable: &Path,
    network: &RemoteNetworkConfig,
    command: RemoteRuntimeOwnerCommandEnvelope,
) -> Result<(), LifecycleError> {
    match try_signal_remote_runtime_owner_command(network, &command) {
        Ok(()) => Ok(()),
        Err(first_error) if remote_runtime_owner_ack_error(&first_error) => {
            let socket_path = remote_runtime_owner_socket_path(network);
            let _ = fs::remove_file(&socket_path);
            ensure_remote_runtime_owner_process_running(current_executable, network)?;
            try_signal_remote_runtime_owner_command(network, &command).map_err(|second_error| {
                LifecycleError::Protocol(format!(
                    "remote runtime owner command failed after protocol restart: {second_error}"
                ))
            })
        }
        Err(error) => Err(error),
    }
}

fn try_signal_remote_runtime_owner_command(
    network: &RemoteNetworkConfig,
    command: &RemoteRuntimeOwnerCommandEnvelope,
) -> Result<(), LifecycleError> {
    let t_total = Instant::now();
    let command_label = remote_runtime_owner_command_label(command);
    let t_connect = Instant::now();
    let mut stream = UnixStream::connect(remote_runtime_owner_socket_path(network))
        .map_err(remote_runtime_owner_error)?;
    ERROR_LOG.log(format!(
        "[diag-newhost] remote_owner signal connect listener={} command={} elapsed={:?} total={:?}",
        network.listener_addr(),
        command_label,
        t_connect.elapsed(),
        t_total.elapsed()
    ));
    let t_write = Instant::now();
    stream
        .write_all(render_remote_runtime_owner_command(command).as_bytes())
        .map_err(remote_runtime_owner_error)?;
    stream.flush().map_err(remote_runtime_owner_error)?;
    stream
        .shutdown(Shutdown::Write)
        .map_err(remote_runtime_owner_error)?;
    ERROR_LOG.log(format!(
        "[diag-newhost] remote_owner signal write listener={} command={} elapsed={:?} total={:?}",
        network.listener_addr(),
        command_label,
        t_write.elapsed(),
        t_total.elapsed()
    ));

    let mut response = String::new();
    let t_read = Instant::now();
    stream
        .read_to_string(&mut response)
        .map_err(remote_runtime_owner_error)?;
    ERROR_LOG.log(format!(
        "[diag-newhost] remote_owner signal read listener={} command={} bytes={} elapsed={:?} total={:?}",
        network.listener_addr(),
        command_label,
        response.len(),
        t_read.elapsed(),
        t_total.elapsed()
    ));
    if response.trim() == "ok" {
        Ok(())
    } else {
        Err(LifecycleError::Protocol(format!(
            "remote runtime owner command was not acknowledged: `{}`",
            response.trim()
        )))
    }
}

fn remote_runtime_owner_ack_error(error: &LifecycleError) -> bool {
    match error {
        LifecycleError::Protocol(message) => {
            message.starts_with("remote runtime owner command was not acknowledged")
        }
        _ => false,
    }
}

fn render_remote_runtime_owner_command(command: &RemoteRuntimeOwnerCommandEnvelope) -> String {
    match command {
        RemoteRuntimeOwnerCommandEnvelope::UpsertSession { node_id, session } => format!(
            "upsert_session\t{}\t{}\n",
            escape_field(node_id),
            render_session_record(session)
        ),
        RemoteRuntimeOwnerCommandEnvelope::RemoveSession {
            node_id,
            authority_id,
            transport_session_id,
        } => format!(
            "remove_session\t{}\t{}\t{}\n",
            escape_field(node_id),
            escape_field(authority_id),
            escape_field(transport_session_id)
        ),
        RemoteRuntimeOwnerCommandEnvelope::RemoveNode { node_id } => {
            format!("remove_node\t{}\n", escape_field(node_id))
        }
        RemoteRuntimeOwnerCommandEnvelope::MarkNodeOffline { node_id } => {
            format!("mark_node_offline\t{}\n", escape_field(node_id))
        }
        RemoteRuntimeOwnerCommandEnvelope::Snapshot => "snapshot\n".to_string(),
    }
}

fn read_remote_runtime_owner_command(
    reader: &mut impl Read,
) -> Result<RemoteRuntimeOwnerCommandEnvelope, LifecycleError> {
    let mut bytes = Vec::new();
    reader
        .read_to_end(&mut bytes)
        .map_err(remote_runtime_owner_error)?;
    let line = String::from_utf8(bytes).map_err(remote_runtime_owner_error)?;
    parse_remote_runtime_owner_command(line.trim())
}

fn parse_remote_runtime_owner_command(
    line: &str,
) -> Result<RemoteRuntimeOwnerCommandEnvelope, LifecycleError> {
    let mut parts = line.split('\t');
    match parts.next().unwrap_or_default() {
        "upsert_session" => {
            let node_id = unescape_field(parts.next().ok_or_else(|| {
                LifecycleError::Protocol("upsert_session is missing node id".to_string())
            })?)?;
            let session = parse_session_record(parts)?;
            Ok(RemoteRuntimeOwnerCommandEnvelope::UpsertSession { node_id, session })
        }
        "remove_session" => {
            let node_id = unescape_field(parts.next().ok_or_else(|| {
                LifecycleError::Protocol("remove_session is missing node id".to_string())
            })?)?;
            let authority_id = unescape_field(parts.next().ok_or_else(|| {
                LifecycleError::Protocol("remove_session is missing authority id".to_string())
            })?)?;
            let transport_session_id = unescape_field(parts.next().ok_or_else(|| {
                LifecycleError::Protocol(
                    "remove_session is missing transport session id".to_string(),
                )
            })?)?;
            if parts.next().is_some() {
                return Err(LifecycleError::Protocol(
                    "remove_session contains unexpected extra fields".to_string(),
                ));
            }
            Ok(RemoteRuntimeOwnerCommandEnvelope::RemoveSession {
                node_id,
                authority_id,
                transport_session_id,
            })
        }
        "remove_node" => {
            let node_id = unescape_field(parts.next().ok_or_else(|| {
                LifecycleError::Protocol("remove_node is missing node id".to_string())
            })?)?;
            if parts.next().is_some() {
                return Err(LifecycleError::Protocol(
                    "remove_node contains unexpected extra fields".to_string(),
                ));
            }
            Ok(RemoteRuntimeOwnerCommandEnvelope::RemoveNode { node_id })
        }
        "mark_node_offline" => {
            let node_id = unescape_field(parts.next().ok_or_else(|| {
                LifecycleError::Protocol("mark_node_offline is missing node id".to_string())
            })?)?;
            if parts.next().is_some() {
                return Err(LifecycleError::Protocol(
                    "mark_node_offline contains unexpected extra fields".to_string(),
                ));
            }
            Ok(RemoteRuntimeOwnerCommandEnvelope::MarkNodeOffline { node_id })
        }
        "snapshot" => Ok(RemoteRuntimeOwnerCommandEnvelope::Snapshot),
        other => Err(LifecycleError::Protocol(format!(
            "unsupported remote runtime owner command `{other}`"
        ))),
    }
}

fn render_remote_runtime_owner_snapshot(records: &[OwnerStateRecord]) -> String {
    let mut lines = Vec::with_capacity(records.len() + 1);
    lines.push("snapshot".to_string());
    for record in records {
        lines.push(format!(
            "{}\t{}",
            escape_field(&record.node_id),
            render_session_record(&record.session)
        ));
    }
    lines.join("\n")
}

fn parse_remote_runtime_owner_snapshot(
    payload: &str,
) -> Result<RemoteRuntimeOwnerSnapshot, LifecycleError> {
    let mut lines = BufReader::new(payload.as_bytes()).lines();
    let header = lines
        .next()
        .transpose()
        .map_err(remote_runtime_owner_error)?
        .unwrap_or_default();
    if header.trim() != "snapshot" {
        return Err(LifecycleError::Protocol(format!(
            "unsupported remote runtime owner snapshot header `{header}`"
        )));
    }
    let mut sessions = Vec::new();
    for line in lines {
        let line = line.map_err(remote_runtime_owner_error)?;
        if line.trim().is_empty() {
            continue;
        }
        let mut parts = line.split('\t');
        let _node_id = unescape_field(parts.next().ok_or_else(|| {
            LifecycleError::Protocol("snapshot row is missing node id".to_string())
        })?)?;
        sessions.push(parse_session_record(parts)?);
    }
    Ok(RemoteRuntimeOwnerSnapshot { sessions })
}

fn owned_record_key(node_id: &str, target_id: &str) -> String {
    format!("{node_id}\t{target_id}")
}

fn render_session_record(session: &ManagedSessionRecord) -> String {
    let current_path = session
        .current_path
        .as_ref()
        .map(|path| path.to_string_lossy().into_owned());
    [
        escape_field(session.address.authority_id()),
        escape_field(session.address.session_id()),
        escape_optional_field(session.selector.as_deref()),
        session.availability.as_str().to_string(),
        escape_optional_field(
            session
                .session_role
                .as_ref()
                .map(WorkspaceSessionRole::as_str),
        ),
        escape_optional_field(session.workspace_key.as_deref()),
        escape_optional_field(session.command_name.as_deref()),
        escape_optional_field(current_path.as_deref()),
        session.attached_clients.to_string(),
        session.window_count.to_string(),
        session.task_state.as_str().to_string(),
    ]
    .join("\t")
}

fn parse_session_record<'a>(
    mut parts: impl Iterator<Item = &'a str>,
) -> Result<ManagedSessionRecord, LifecycleError> {
    let authority_id = unescape_field(parts.next().ok_or_else(|| {
        LifecycleError::Protocol("session record is missing authority id".to_string())
    })?)?;
    let session_id = unescape_field(parts.next().ok_or_else(|| {
        LifecycleError::Protocol("session record is missing session id".to_string())
    })?)?;
    let selector = unescape_optional_field(parts.next().ok_or_else(|| {
        LifecycleError::Protocol("session record is missing selector".to_string())
    })?)?;
    let availability = SessionAvailability::parse(parts.next().ok_or_else(|| {
        LifecycleError::Protocol("session record is missing availability".to_string())
    })?)
    .ok_or_else(|| {
        LifecycleError::Protocol("session record has invalid availability".to_string())
    })?;
    let session_role = unescape_optional_field(parts.next().ok_or_else(|| {
        LifecycleError::Protocol("session record is missing session role".to_string())
    })?)?
    .as_deref()
    .and_then(WorkspaceSessionRole::parse);
    let workspace_key = unescape_optional_field(parts.next().ok_or_else(|| {
        LifecycleError::Protocol("session record is missing workspace key".to_string())
    })?)?;
    let command_name = unescape_optional_field(parts.next().ok_or_else(|| {
        LifecycleError::Protocol("session record is missing command name".to_string())
    })?)?;
    let current_path = unescape_optional_field(parts.next().ok_or_else(|| {
        LifecycleError::Protocol("session record is missing current path".to_string())
    })?)?
    .map(PathBuf::from);
    let attached_clients = parts
        .next()
        .ok_or_else(|| {
            LifecycleError::Protocol("session record is missing attached clients".to_string())
        })?
        .parse::<usize>()
        .map_err(remote_runtime_owner_error)?;
    let window_count = parts
        .next()
        .ok_or_else(|| {
            LifecycleError::Protocol("session record is missing window count".to_string())
        })?
        .parse::<usize>()
        .map_err(remote_runtime_owner_error)?;
    let task_state = ManagedSessionTaskState::parse(parts.next().ok_or_else(|| {
        LifecycleError::Protocol("session record is missing task state".to_string())
    })?)
    .ok_or_else(|| LifecycleError::Protocol("session record has invalid task state".to_string()))?;
    if parts.next().is_some() {
        return Err(LifecycleError::Protocol(
            "session record contains unexpected extra fields".to_string(),
        ));
    }

    Ok(ManagedSessionRecord {
        address: ManagedSessionAddress::remote_peer(authority_id, session_id),
        selector,
        availability,
        workspace_dir: None,
        workspace_key,
        session_role,
        opened_by: Vec::new(),
        attached_clients,
        window_count,
        command_name,
        current_path,
        task_state,
    })
}

fn escape_field(value: &str) -> String {
    value.replace('\\', "\\\\").replace('\t', "\\t")
}

fn unescape_field(value: &str) -> Result<String, LifecycleError> {
    let mut result = String::new();
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            result.push(ch);
            continue;
        }
        match chars.next() {
            Some('\\') => result.push('\\'),
            Some('t') => result.push('\t'),
            Some(other) => {
                return Err(LifecycleError::Protocol(format!(
                    "unsupported escape sequence `\\{other}`"
                )))
            }
            None => {
                return Err(LifecycleError::Protocol(
                    "unterminated escape sequence".to_string(),
                ))
            }
        }
    }
    Ok(result)
}

fn escape_optional_field(value: Option<&str>) -> String {
    value.map(escape_field).unwrap_or_else(|| "~".to_string())
}

fn unescape_optional_field(value: &str) -> Result<Option<String>, LifecycleError> {
    if value == "~" {
        return Ok(None);
    }
    unescape_field(value).map(Some)
}

fn sanitize_path_component(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn remote_runtime_owner_error(
    error: impl Into<Box<dyn std::error::Error + Send + Sync>>,
) -> LifecycleError {
    let error = io::Error::new(ErrorKind::Other, error.into().to_string());
    LifecycleError::Io("remote runtime owner operation failed".to_string(), error)
}

#[cfg(test)]
mod tests {
    use super::{
        handle_remote_runtime_owner_client, parse_remote_runtime_owner_command,
        parse_remote_runtime_owner_snapshot, remote_runtime_owner_args,
        remote_runtime_owner_socket_path, render_remote_runtime_owner_command,
        render_remote_runtime_owner_snapshot, OwnerStateRecord, RemoteRuntimeOwnerCommandEnvelope,
        RemoteRuntimeOwnerSharedState, RemoteRuntimeOwnerSnapshot,
    };
    use crate::cli::RemoteNetworkConfig;
    use crate::domain::session_catalog::{
        ManagedSessionAddress, ManagedSessionRecord, ManagedSessionTaskState, SessionAvailability,
    };
    use crate::domain::workspace::WorkspaceSessionRole;
    use std::collections::HashMap;
    use std::io::Write;
    use std::net::Shutdown;
    use std::os::unix::net::UnixStream;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};

    fn remote_session(authority_id: &str, session_id: &str) -> ManagedSessionRecord {
        ManagedSessionRecord {
            address: ManagedSessionAddress::remote_peer(authority_id, session_id),
            selector: Some(format!("{authority_id}:{session_id}")),
            availability: SessionAvailability::Online,
            workspace_dir: None,
            workspace_key: Some("wk".to_string()),
            session_role: Some(WorkspaceSessionRole::TargetHost),
            opened_by: Vec::new(),
            attached_clients: 1,
            window_count: 2,
            command_name: Some("codex".to_string()),
            current_path: Some(PathBuf::from("/tmp/demo")),
            task_state: ManagedSessionTaskState::Input,
        }
    }

    #[test]
    fn remote_runtime_owner_args_include_hidden_command_and_network_flags() {
        let network = RemoteNetworkConfig {
            port: 9001,
            connect: Some("10.0.0.8:7474".to_string()),
            node_id: None,
            public_endpoint: None,
        };

        let args = remote_runtime_owner_args(&network);

        assert_eq!(
            args,
            vec![
                "--port",
                "9001",
                "--connect",
                "10.0.0.8:7474",
                "__remote-runtime-owner",
            ]
        );
    }

    #[test]
    fn remote_runtime_owner_socket_path_is_scoped_to_listener_addr() {
        let network = RemoteNetworkConfig {
            port: 7575,
            connect: None,
            node_id: None,
            public_endpoint: None,
        };
        let path = remote_runtime_owner_socket_path(&network);

        assert_eq!(
            path.file_name().and_then(|value| value.to_str()),
            Some("waitagent-remote-runtime-owner-0_0_0_0_7575.sock")
        );
    }

    #[test]
    fn remote_runtime_owner_command_round_trips_upsert_session() {
        let rendered = render_remote_runtime_owner_command(
            &RemoteRuntimeOwnerCommandEnvelope::UpsertSession {
                node_id: "peer-a".to_string(),
                session: remote_session("peer-a", "pty1"),
            },
        );

        let parsed =
            parse_remote_runtime_owner_command(rendered.trim()).expect("command should parse");

        match parsed {
            RemoteRuntimeOwnerCommandEnvelope::UpsertSession { node_id, session } => {
                assert_eq!(node_id, "peer-a");
                assert_eq!(session.address.authority_id(), "peer-a");
                assert_eq!(session.address.session_id(), "pty1");
                assert_eq!(session.command_name.as_deref(), Some("codex"));
            }
            other => panic!("unexpected parsed command: {other:?}"),
        }
    }

    #[test]
    fn remote_runtime_owner_command_round_trips_remove_node() {
        let rendered =
            render_remote_runtime_owner_command(&RemoteRuntimeOwnerCommandEnvelope::RemoveNode {
                node_id: "peer-a".to_string(),
            });

        let parsed =
            parse_remote_runtime_owner_command(rendered.trim()).expect("command should parse");

        match parsed {
            RemoteRuntimeOwnerCommandEnvelope::RemoveNode { node_id } => {
                assert_eq!(node_id, "peer-a");
            }
            other => panic!("unexpected parsed command: {other:?}"),
        }
    }

    #[test]
    fn remote_runtime_owner_command_round_trips_mark_node_offline() {
        let rendered = render_remote_runtime_owner_command(
            &RemoteRuntimeOwnerCommandEnvelope::MarkNodeOffline {
                node_id: "peer-a".to_string(),
            },
        );

        let parsed =
            parse_remote_runtime_owner_command(rendered.trim()).expect("command should parse");

        match parsed {
            RemoteRuntimeOwnerCommandEnvelope::MarkNodeOffline { node_id } => {
                assert_eq!(node_id, "peer-a");
            }
            other => panic!("unexpected parsed command: {other:?}"),
        }
    }

    #[test]
    fn remote_runtime_owner_snapshot_round_trips_sessions() {
        let payload = render_remote_runtime_owner_snapshot(&[super::OwnerStateRecord {
            node_id: "peer-a".to_string(),
            session: remote_session("peer-a", "pty1"),
        }]);

        let snapshot =
            parse_remote_runtime_owner_snapshot(&payload).expect("snapshot should parse");

        assert_eq!(
            snapshot,
            RemoteRuntimeOwnerSnapshot {
                sessions: vec![remote_session("peer-a", "pty1")]
            }
        );
    }

    #[test]
    fn remove_node_command_drops_all_sessions_for_matching_node() {
        let state = RemoteRuntimeOwnerSharedState {
            records: Arc::new(Mutex::new(HashMap::from([
                (
                    "peer-a\tremote-peer:peer-a:pty1".to_string(),
                    OwnerStateRecord {
                        node_id: "peer-a".to_string(),
                        session: remote_session("peer-a", "pty1"),
                    },
                ),
                (
                    "peer-a\tremote-peer:peer-a:pty2".to_string(),
                    OwnerStateRecord {
                        node_id: "peer-a".to_string(),
                        session: remote_session("peer-a", "pty2"),
                    },
                ),
                (
                    "peer-b\tremote-peer:peer-b:pty9".to_string(),
                    OwnerStateRecord {
                        node_id: "peer-b".to_string(),
                        session: remote_session("peer-b", "pty9"),
                    },
                ),
            ]))),
            running: Arc::new(AtomicBool::new(true)),
        };
        let (mut client, mut server) = UnixStream::pair().expect("unix stream pair should open");
        client
            .write_all(
                render_remote_runtime_owner_command(
                    &RemoteRuntimeOwnerCommandEnvelope::RemoveNode {
                        node_id: "peer-a".to_string(),
                    },
                )
                .as_bytes(),
            )
            .expect("command should write");
        client
            .shutdown(Shutdown::Write)
            .expect("client shutdown should succeed");

        let response =
            handle_remote_runtime_owner_client(&state, &mut server).expect("command should handle");

        assert_eq!(response.as_deref(), Some("ok\n"));
        let records = state
            .records
            .lock()
            .expect("remote runtime owner state mutex should not be poisoned");
        assert_eq!(records.len(), 1);
        assert!(records.contains_key("peer-b\tremote-peer:peer-b:pty9"));
    }

    #[test]
    fn mark_node_offline_command_keeps_sessions_and_marks_matching_node_offline() {
        let state = RemoteRuntimeOwnerSharedState {
            records: Arc::new(Mutex::new(HashMap::from([
                (
                    "peer-a\tremote-peer:peer-a:pty1".to_string(),
                    OwnerStateRecord {
                        node_id: "peer-a".to_string(),
                        session: remote_session("peer-a", "pty1"),
                    },
                ),
                (
                    "peer-a\tremote-peer:peer-a:pty2".to_string(),
                    OwnerStateRecord {
                        node_id: "peer-a".to_string(),
                        session: remote_session("peer-a", "pty2"),
                    },
                ),
                (
                    "peer-b\tremote-peer:peer-b:pty9".to_string(),
                    OwnerStateRecord {
                        node_id: "peer-b".to_string(),
                        session: remote_session("peer-b", "pty9"),
                    },
                ),
            ]))),
            running: Arc::new(AtomicBool::new(true)),
        };
        let (mut client, mut server) = UnixStream::pair().expect("unix stream pair should open");
        client
            .write_all(
                render_remote_runtime_owner_command(
                    &RemoteRuntimeOwnerCommandEnvelope::MarkNodeOffline {
                        node_id: "peer-a".to_string(),
                    },
                )
                .as_bytes(),
            )
            .expect("command should write");
        client
            .shutdown(Shutdown::Write)
            .expect("client shutdown should succeed");

        let response =
            handle_remote_runtime_owner_client(&state, &mut server).expect("command should handle");

        assert_eq!(response.as_deref(), Some("ok\n"));
        let records = state
            .records
            .lock()
            .expect("remote runtime owner state mutex should not be poisoned");
        assert_eq!(records.len(), 3);
        assert_eq!(
            records["peer-a\tremote-peer:peer-a:pty1"]
                .session
                .availability,
            SessionAvailability::Offline
        );
        assert_eq!(
            records["peer-a\tremote-peer:peer-a:pty2"]
                .session
                .availability,
            SessionAvailability::Offline
        );
        assert_eq!(
            records["peer-b\tremote-peer:peer-b:pty9"]
                .session
                .availability,
            SessionAvailability::Online
        );
    }

    #[test]
    fn remote_runtime_owner_socket_path_lives_in_tmp() {
        let network = RemoteNetworkConfig {
            port: 7474,
            connect: None,
            node_id: None,
            public_endpoint: None,
        };
        let path = remote_runtime_owner_socket_path(&network);

        assert_eq!(path.parent(), Some(Path::new("/tmp")));
    }
}
