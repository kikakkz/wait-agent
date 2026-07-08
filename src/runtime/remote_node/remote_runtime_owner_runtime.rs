use crate::cli::{prepend_global_network_args, RemoteNetworkConfig, RemoteRuntimeOwnerCommand};
use crate::domain::session_catalog::{
    ManagedSessionAddress, ManagedSessionRecord, ManagedSessionTaskState, SessionAvailability,
};
use crate::domain::workspace::WorkspaceSessionRole;
use crate::infra::error_log::ERROR_LOG;
use crate::infra::tmux::{EmbeddedTmuxBackend, TmuxSessionGateway, TmuxSocketName};
use crate::lifecycle::LifecycleError;
use crate::runtime::current_executable::current_waitagent_executable;
use crate::runtime::remote_node::remote_workspace_socket_registry_runtime::live_workspace_socket_names_for_network;
use crate::runtime::sidecar_process_runtime::spawn_waitagent_sidecar_child;
use crate::runtime::workspace::sidecar_process_runtime::spawn_waitagent_sidecar;
use std::collections::HashMap;
use std::fs;
use std::io::{self, BufRead, BufReader, ErrorKind, Read, Write};
use std::net::Shutdown;
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixListener as TokioUnixListener;
use tokio::net::UnixStream as TokioUnixStream;
use tokio::time::{sleep_until, Instant as TokioInstant};

#[cfg(not(test))]
const OFFLINE_NODE_RETENTION: Duration = Duration::from_secs(120);
#[cfg(test)]
const OFFLINE_NODE_RETENTION: Duration = Duration::from_millis(10);

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
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OwnerStateRecord {
    node_id: String,
    session: ManagedSessionRecord,
}

#[derive(Clone)]
struct RemoteRuntimeOwnerSharedState {
    records: Arc<Mutex<HashMap<String, OwnerStateRecord>>>,
    offline_nodes: Arc<Mutex<HashMap<String, Instant>>>,
    running: Arc<AtomicBool>,
    network: RemoteNetworkConfig,
    current_executable: PathBuf,
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

    pub fn run_owner(&self, command: RemoteRuntimeOwnerCommand) -> Result<(), LifecycleError> {
        let socket_path = remote_runtime_owner_socket_path(&self.network);
        if socket_path.exists() {
            let _ = fs::remove_file(&socket_path);
        }

        let state = RemoteRuntimeOwnerSharedState {
            records: Arc::new(Mutex::new(HashMap::new())),
            offline_nodes: Arc::new(Mutex::new(HashMap::new())),
            running: Arc::new(AtomicBool::new(true)),
            network: self.network.clone(),
            current_executable: self.current_executable.clone(),
        };

        let runtime = tokio::runtime::Runtime::new().map_err(remote_runtime_owner_error)?;
        let listener = match runtime
            .block_on(async { TokioUnixListener::bind(&socket_path) })
            .map_err(remote_runtime_owner_error)
        {
            Ok(listener) => {
                if let Err(error) =
                    notify_remote_runtime_owner_ready(command.ready_socket.as_deref(), Ok(()))
                {
                    ERROR_LOG.log(format!(
                        "[diag-newhost] remote_runtime_owner ready notification failed: {error}"
                    ));
                }
                listener
            }
            Err(error) => {
                let _ = notify_remote_runtime_owner_ready(
                    command.ready_socket.as_deref(),
                    Err(error.to_string()),
                );
                return Err(error);
            }
        };

        let result = runtime.block_on(run_remote_runtime_owner_event_loop(listener, state.clone()));

        state.running.store(false, Ordering::Relaxed);
        let _ = UnixStream::connect(&socket_path);
        let _ = fs::remove_file(&socket_path);
        result
    }
}

async fn run_remote_runtime_owner_event_loop(
    listener: TokioUnixListener,
    state: RemoteRuntimeOwnerSharedState,
) -> Result<(), LifecycleError> {
    let mut next_ttl_deadline: Option<TokioInstant> = compute_next_ttl_deadline(&state);

    loop {
        if !state.running.load(Ordering::Relaxed) {
            break;
        }

        tokio::select! {
            result = listener.accept() => {
                let (mut stream, _) = result.map_err(remote_runtime_owner_error)?;
                let t_client = Instant::now();
                ERROR_LOG.log("[diag-newhost] remote_owner server accepted".to_string());
                match handle_remote_runtime_owner_client_async(&state, &mut stream).await {
                    Ok(Some(payload)) => {
                        let t_write = Instant::now();
                        let write_ok = stream.write_all(payload.as_bytes()).await.is_ok();
                        let flush_ok = stream.flush().await.is_ok();
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
                next_ttl_deadline = compute_next_ttl_deadline(&state);
            }
            _ = sleep_until(next_ttl_deadline.unwrap_or_else(|| TokioInstant::now() + Duration::from_secs(86400))), if next_ttl_deadline.is_some() => {
                let pruned = prune_expired_offline_nodes(&state, Instant::now());
                if !pruned.is_empty() {
                    if let Err(error) = emit_remote_target_exited_cleanup(&state, &pruned) {
                        ERROR_LOG.log(format!(
                            "[diag-newhost] remote_owner cleanup error: {error}"
                        ));
                    }
                }
                next_ttl_deadline = compute_next_ttl_deadline(&state);
            }
        }
    }
    Ok(())
}

async fn handle_remote_runtime_owner_client_async(
    state: &RemoteRuntimeOwnerSharedState,
    stream: &mut TokioUnixStream,
) -> Result<Option<String>, LifecycleError> {
    let t_total = Instant::now();
    let command = read_remote_runtime_owner_command_async(stream).await?;
    let command_label = remote_runtime_owner_command_label(&command);
    ERROR_LOG.log(format!(
        "[diag-newhost] remote_owner server read_command command={} elapsed={:?}",
        command_label,
        t_total.elapsed()
    ));
    let t_handle = Instant::now();
    let response = handle_remote_runtime_owner_command(state, command);
    ERROR_LOG.log(format!(
        "[diag-newhost] remote_owner server handled command={} ok={} elapsed={:?} total={:?}",
        command_label,
        response.is_ok(),
        t_handle.elapsed(),
        t_total.elapsed()
    ));
    response
}

async fn read_remote_runtime_owner_command_async(
    reader: &mut TokioUnixStream,
) -> Result<RemoteRuntimeOwnerCommandEnvelope, LifecycleError> {
    let mut bytes = Vec::new();
    reader
        .read_to_end(&mut bytes)
        .await
        .map_err(remote_runtime_owner_error)?;
    let line = String::from_utf8(bytes).map_err(remote_runtime_owner_error)?;
    parse_remote_runtime_owner_command(line.trim())
}

fn compute_next_ttl_deadline(state: &RemoteRuntimeOwnerSharedState) -> Option<TokioInstant> {
    let offline_nodes = state
        .offline_nodes
        .lock()
        .expect("remote runtime owner offline node mutex should not be poisoned");
    let now = Instant::now();
    offline_nodes
        .values()
        .map(|since| *since + OFFLINE_NODE_RETENTION)
        .filter(|deadline| *deadline > now)
        .min()
        .map(|deadline| {
            let until = deadline.duration_since(now);
            TokioInstant::now() + until
        })
}

fn emit_remote_target_exited_cleanup(
    state: &RemoteRuntimeOwnerSharedState,
    pruned_records: &[OwnerStateRecord],
) -> Result<(), LifecycleError> {
    if pruned_records.is_empty() {
        return Ok(());
    }

    let live_sockets = live_workspace_socket_names_for_network(&state.network)?;
    if live_sockets.is_empty() {
        return Ok(());
    }

    let backend = EmbeddedTmuxBackend::from_build_env().map_err(remote_runtime_owner_error)?;

    for record in pruned_records {
        let target = record.session.address.qualified_target();
        for socket_name in &live_sockets {
            let sessions = backend
                .list_sessions_on_socket(&TmuxSocketName::new(socket_name))
                .map_err(remote_runtime_owner_error)?;
            for session in sessions {
                if !session.is_workspace_chrome() {
                    continue;
                }
                let args = vec![
                    "__remote-target-exited".to_string(),
                    "--socket-name".to_string(),
                    socket_name.clone(),
                    "--session-name".to_string(),
                    session.address.session_id().to_string(),
                    "--target".to_string(),
                    target.clone(),
                ];
                spawn_waitagent_sidecar(&state.current_executable, args)
                    .map_err(remote_runtime_owner_error)?;
            }
        }
    }
    Ok(())
}

impl RemoteRuntimeOwnerRuntime {
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

    pub fn shutdown_owner_if_unused(network: &RemoteNetworkConfig) -> Result<(), LifecycleError> {
        let sockets =
            crate::runtime::remote_workspace_socket_registry_runtime::live_workspace_socket_names_for_network(network)?;
        if !sockets.is_empty() {
            return Ok(());
        }
        try_signal_remote_runtime_owner_command(
            network,
            &RemoteRuntimeOwnerCommandEnvelope::Shutdown,
        )
        .or_else(|error| {
            if remote_runtime_owner_unavailable_error(&error) {
                Ok(())
            } else {
                Err(error)
            }
        })
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
        offline_nodes: Arc::new(Mutex::new(HashMap::new())),
        running: Arc::new(AtomicBool::new(true)),
        network: network.clone(),
        current_executable: PathBuf::from("/tmp/waitagent-test"),
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

#[cfg(test)]
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
    let response = handle_remote_runtime_owner_command(state, command);
    ERROR_LOG.log(format!(
        "[diag-newhost] remote_owner server handled command={} ok={} elapsed={:?} total={:?}",
        command_label,
        response.is_ok(),
        t_handle.elapsed(),
        t_total.elapsed()
    ));
    response
}

fn handle_remote_runtime_owner_command(
    state: &RemoteRuntimeOwnerSharedState,
    command: RemoteRuntimeOwnerCommandEnvelope,
) -> Result<Option<String>, LifecycleError> {
    match command {
        RemoteRuntimeOwnerCommandEnvelope::UpsertSession { node_id, session } => {
            let key = owned_record_key(&node_id, session.address.id().as_str());
            state
                .offline_nodes
                .lock()
                .expect("remote runtime owner offline node mutex should not be poisoned")
                .remove(&node_id);
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
            clear_offline_node_if_empty(state, &node_id);
            Ok(Some("ok\n".to_string()))
        }
        RemoteRuntimeOwnerCommandEnvelope::RemoveNode { node_id } => {
            let mut guard = state
                .records
                .lock()
                .expect("remote runtime owner state mutex should not be poisoned");
            guard.retain(|_, record| record.node_id != node_id);
            state
                .offline_nodes
                .lock()
                .expect("remote runtime owner offline node mutex should not be poisoned")
                .remove(&node_id);
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
            if guard.values().any(|record| record.node_id == node_id) {
                state
                    .offline_nodes
                    .lock()
                    .expect("remote runtime owner offline node mutex should not be poisoned")
                    .entry(node_id)
                    .or_insert_with(Instant::now);
            }
            Ok(Some("ok\n".to_string()))
        }
        RemoteRuntimeOwnerCommandEnvelope::Snapshot => {
            let pruned = prune_expired_offline_nodes(state, Instant::now());
            if !pruned.is_empty() {
                if let Err(error) = emit_remote_target_exited_cleanup(state, &pruned) {
                    ERROR_LOG.log(format!(
                        "[diag-newhost] remote_owner snapshot cleanup error: {error}"
                    ));
                }
            }
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
        RemoteRuntimeOwnerCommandEnvelope::Shutdown => {
            state.running.store(false, Ordering::Relaxed);
            Ok(Some("ok\n".to_string()))
        }
    }
}

fn remote_runtime_owner_command_label(command: &RemoteRuntimeOwnerCommandEnvelope) -> &'static str {
    match command {
        RemoteRuntimeOwnerCommandEnvelope::UpsertSession { .. } => "upsert_session",
        RemoteRuntimeOwnerCommandEnvelope::RemoveSession { .. } => "remove_session",
        RemoteRuntimeOwnerCommandEnvelope::RemoveNode { .. } => "remove_node",
        RemoteRuntimeOwnerCommandEnvelope::MarkNodeOffline { .. } => "mark_node_offline",
        RemoteRuntimeOwnerCommandEnvelope::Snapshot => "snapshot",
        RemoteRuntimeOwnerCommandEnvelope::Shutdown => "shutdown",
    }
}

fn prune_expired_offline_nodes(
    state: &RemoteRuntimeOwnerSharedState,
    now: Instant,
) -> Vec<OwnerStateRecord> {
    let expired_nodes = {
        let offline_nodes = state
            .offline_nodes
            .lock()
            .expect("remote runtime owner offline node mutex should not be poisoned");
        offline_nodes
            .iter()
            .filter_map(|(node_id, since)| {
                (now.duration_since(*since) >= OFFLINE_NODE_RETENTION).then(|| node_id.clone())
            })
            .collect::<Vec<_>>()
    };
    if expired_nodes.is_empty() {
        return Vec::new();
    }

    let expired_set: std::collections::HashSet<_> = expired_nodes.iter().cloned().collect();
    let mut records = state
        .records
        .lock()
        .expect("remote runtime owner state mutex should not be poisoned");
    let pruned: Vec<OwnerStateRecord> = records
        .values()
        .filter(|record| expired_set.contains(&record.node_id))
        .cloned()
        .collect();
    records.retain(|_, record| !expired_set.contains(&record.node_id));
    drop(records);

    let mut offline_nodes = state
        .offline_nodes
        .lock()
        .expect("remote runtime owner offline node mutex should not be poisoned");
    for node_id in expired_nodes {
        offline_nodes.remove(&node_id);
    }

    pruned
}

fn clear_offline_node_if_empty(state: &RemoteRuntimeOwnerSharedState, node_id: &str) {
    let has_records = state
        .records
        .lock()
        .expect("remote runtime owner state mutex should not be poisoned")
        .values()
        .any(|record| record.node_id == node_id);
    if !has_records {
        state
            .offline_nodes
            .lock()
            .expect("remote runtime owner offline node mutex should not be poisoned")
            .remove(node_id);
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
    let lock_path = remote_runtime_owner_startup_lock_path(&socket_path);
    let Some(_startup_lock) = RemoteRuntimeOwnerStartupLock::try_acquire(&lock_path)? else {
        let _startup_lock = RemoteRuntimeOwnerStartupLock::acquire(&lock_path)?;
        if remote_runtime_owner_available(&socket_path) {
            return Ok(());
        }
        return Err(LifecycleError::Protocol(format!(
            "remote runtime owner for listener `{}` was not ready after startup lock {} released",
            network.listener_addr(),
            lock_path.display()
        )));
    };
    if remote_runtime_owner_available(&socket_path) {
        return Ok(());
    }
    if socket_path.exists() {
        let _ = fs::remove_file(&socket_path);
    }

    let ready_socket = remote_runtime_owner_ready_socket_path(&socket_path);
    if ready_socket.exists() {
        let _ = fs::remove_file(&ready_socket);
    }
    let ready_listener = UnixListener::bind(&ready_socket).map_err(remote_runtime_owner_error)?;

    let child = spawn_waitagent_sidecar_child(
        current_executable,
        remote_runtime_owner_args(network, Some(&ready_socket)),
    )
    .map_err(remote_runtime_owner_error)?;
    let ready = wait_for_remote_runtime_owner_ready(ready_listener, &ready_socket, child);
    let _ = fs::remove_file(&ready_socket);
    ready
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

struct RemoteRuntimeOwnerStartupLock {
    _file: fs::File,
}

impl RemoteRuntimeOwnerStartupLock {
    fn try_acquire(path: &Path) -> Result<Option<Self>, LifecycleError> {
        let file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(remote_runtime_owner_error)?;
        match flock_remote_runtime_owner_startup_lock(&file, libc::LOCK_EX | libc::LOCK_NB) {
            Ok(()) => Ok(Some(Self { _file: file })),
            Err(error) if error.kind() == ErrorKind::WouldBlock => Ok(None),
            Err(error) => Err(remote_runtime_owner_error(error)),
        }
    }

    fn acquire(path: &Path) -> Result<Self, LifecycleError> {
        let file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(remote_runtime_owner_error)?;
        flock_remote_runtime_owner_startup_lock(&file, libc::LOCK_EX)
            .map_err(remote_runtime_owner_error)?;
        Ok(Self { _file: file })
    }
}

fn remote_runtime_owner_startup_lock_path(socket_path: &Path) -> PathBuf {
    socket_path.with_extension("sock.lock")
}

fn flock_remote_runtime_owner_startup_lock(
    file: &fs::File,
    operation: libc::c_int,
) -> io::Result<()> {
    if unsafe { libc::flock(file.as_raw_fd(), operation) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(crate) fn remote_runtime_owner_socket_path(network: &RemoteNetworkConfig) -> PathBuf {
    std::env::temp_dir().join(format!(
        "waitagent-remote-runtime-owner-{}.sock",
        sanitize_path_component(&network.listener_addr().to_string())
    ))
}

fn remote_runtime_owner_ready_socket_path(owner_socket_path: &Path) -> PathBuf {
    let pid = std::process::id();
    owner_socket_path.with_extension(format!("ready-{pid}.sock"))
}

fn notify_remote_runtime_owner_ready(
    ready_socket: Option<&str>,
    result: Result<(), String>,
) -> io::Result<()> {
    let Some(ready_socket) = ready_socket else {
        return Ok(());
    };
    let mut stream = UnixStream::connect(ready_socket)?;
    match result {
        Ok(()) => stream.write_all(b"ok\n")?,
        Err(error) => {
            stream.write_all(b"err\t")?;
            stream.write_all(error.as_bytes())?;
            stream.write_all(b"\n")?;
        }
    }
    stream.flush()
}

fn wait_for_remote_runtime_owner_ready(
    listener: UnixListener,
    ready_socket: &Path,
    mut child: std::process::Child,
) -> Result<(), LifecycleError> {
    enum RemoteRuntimeOwnerReadyEvent {
        Ready(io::Result<String>),
        Exited(io::Result<std::process::ExitStatus>),
    }

    let (event_tx, event_rx) = mpsc::channel();
    let ready_tx = event_tx.clone();
    thread::spawn(move || {
        let response = listener.accept().and_then(|(mut stream, _)| {
            let mut response = String::new();
            stream.read_to_string(&mut response)?;
            Ok(response)
        });
        let _ = ready_tx.send(RemoteRuntimeOwnerReadyEvent::Ready(response));
    });

    thread::spawn(move || {
        let status = child.wait();
        let _ = event_tx.send(RemoteRuntimeOwnerReadyEvent::Exited(status));
    });

    loop {
        match event_rx.recv() {
            Ok(RemoteRuntimeOwnerReadyEvent::Ready(Ok(response))) => {
                let response = response.trim();
                if response == "ok" {
                    return Ok(());
                }
                if let Some(error) = response.strip_prefix("err\t") {
                    return Err(LifecycleError::Protocol(format!(
                        "remote runtime owner failed to start: {error}"
                    )));
                }
                return Err(LifecycleError::Protocol(format!(
                    "remote runtime owner sent invalid ready response `{response}`"
                )));
            }
            Ok(RemoteRuntimeOwnerReadyEvent::Ready(Err(error))) => {
                return Err(remote_runtime_owner_error(error));
            }
            Ok(RemoteRuntimeOwnerReadyEvent::Exited(Ok(status))) => {
                return Err(LifecycleError::Protocol(format!(
                    "remote runtime owner exited before reporting ready: {status}"
                )));
            }
            Ok(RemoteRuntimeOwnerReadyEvent::Exited(Err(error))) => {
                return Err(remote_runtime_owner_error(error));
            }
            Err(_) => {
                return Err(LifecycleError::Protocol(format!(
                    "remote runtime owner ready socket `{}` closed before reporting ready",
                    ready_socket.display()
                )));
            }
        }
    }
}

pub(crate) fn remote_runtime_owner_args(
    network: &RemoteNetworkConfig,
    ready_socket: Option<&Path>,
) -> Vec<String> {
    let mut args = vec!["__remote-runtime-owner".to_string()];
    if let Some(ready_socket) = ready_socket {
        args.push("--ready-socket".to_string());
        args.push(ready_socket.display().to_string());
    }
    prepend_global_network_args(args, network)
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
        .map_err(remote_runtime_owner_io_error)?;
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
        .map_err(remote_runtime_owner_io_error)?;
    stream.flush().map_err(remote_runtime_owner_io_error)?;
    stream
        .shutdown(Shutdown::Write)
        .map_err(remote_runtime_owner_io_error)?;
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
        .map_err(remote_runtime_owner_io_error)?;
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

fn remote_runtime_owner_unavailable_error(error: &LifecycleError) -> bool {
    match error {
        LifecycleError::Io(_, error) => matches!(
            error.kind(),
            ErrorKind::NotFound
                | ErrorKind::ConnectionRefused
                | ErrorKind::ConnectionReset
                | ErrorKind::ConnectionAborted
                | ErrorKind::BrokenPipe
        ),
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
        RemoteRuntimeOwnerCommandEnvelope::Shutdown => "shutdown\n".to_string(),
    }
}

#[cfg(test)]
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
        "snapshot" => {
            if parts.next().is_some() {
                return Err(LifecycleError::Protocol(
                    "snapshot contains unexpected extra fields".to_string(),
                ));
            }
            Ok(RemoteRuntimeOwnerCommandEnvelope::Snapshot)
        }
        "shutdown" => {
            if parts.next().is_some() {
                return Err(LifecycleError::Protocol(
                    "shutdown contains unexpected extra fields".to_string(),
                ));
            }
            Ok(RemoteRuntimeOwnerCommandEnvelope::Shutdown)
        }
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

fn remote_runtime_owner_io_error(error: io::Error) -> LifecycleError {
    LifecycleError::Io("remote runtime owner operation failed".to_string(), error)
}

#[cfg(test)]
mod tests {
    use super::{
        handle_remote_runtime_owner_client, parse_remote_runtime_owner_command,
        parse_remote_runtime_owner_snapshot, prune_expired_offline_nodes,
        remote_runtime_owner_args, remote_runtime_owner_socket_path,
        render_remote_runtime_owner_command, render_remote_runtime_owner_snapshot,
        run_remote_runtime_owner_event_loop, OwnerStateRecord, RemoteRuntimeOwnerCommandEnvelope,
        RemoteRuntimeOwnerSharedState, RemoteRuntimeOwnerSnapshot, OFFLINE_NODE_RETENTION,
    };
    use crate::cli::RemoteNetworkConfig;
    use crate::domain::session_catalog::{
        ManagedSessionAddress, ManagedSessionRecord, ManagedSessionTaskState, SessionAvailability,
    };
    use crate::domain::workspace::WorkspaceSessionRole;
    use std::collections::HashMap;
    use std::io::{Read, Write};
    use std::net::Shutdown;
    use std::os::unix::net::UnixStream;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

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

        let args = remote_runtime_owner_args(&network, None);

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
    fn remote_runtime_owner_args_include_ready_socket_when_requested() {
        let network = RemoteNetworkConfig {
            port: 9001,
            connect: Some("10.0.0.8:7474".to_string()),
            node_id: None,
            public_endpoint: None,
        };

        let args = remote_runtime_owner_args(&network, Some(Path::new("/tmp/runtime-ready.sock")));

        assert!(args.iter().any(|arg| arg == "--ready-socket"));
        assert!(args.iter().any(|arg| arg == "/tmp/runtime-ready.sock"));
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
    fn remote_runtime_owner_command_round_trips_shutdown() {
        let rendered =
            render_remote_runtime_owner_command(&RemoteRuntimeOwnerCommandEnvelope::Shutdown);

        let parsed =
            parse_remote_runtime_owner_command(rendered.trim()).expect("command should parse");

        assert_eq!(parsed, RemoteRuntimeOwnerCommandEnvelope::Shutdown);
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
            offline_nodes: Arc::new(Mutex::new(HashMap::new())),
            running: Arc::new(AtomicBool::new(true)),
            network: RemoteNetworkConfig {
                port: 0,
                connect: None,
                node_id: None,
                public_endpoint: None,
            },
            current_executable: PathBuf::from("/tmp/waitagent-test"),
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
            offline_nodes: Arc::new(Mutex::new(HashMap::new())),
            running: Arc::new(AtomicBool::new(true)),
            network: RemoteNetworkConfig {
                port: 0,
                connect: None,
                node_id: None,
                public_endpoint: None,
            },
            current_executable: PathBuf::from("/tmp/waitagent-test"),
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
        assert!(state
            .offline_nodes
            .lock()
            .expect("remote runtime owner offline node mutex should not be poisoned")
            .contains_key("peer-a"));
    }

    #[test]
    fn expired_offline_node_is_pruned_from_snapshot_source() {
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
                    "peer-b\tremote-peer:peer-b:pty9".to_string(),
                    OwnerStateRecord {
                        node_id: "peer-b".to_string(),
                        session: remote_session("peer-b", "pty9"),
                    },
                ),
            ]))),
            offline_nodes: Arc::new(Mutex::new(HashMap::from([(
                "peer-a".to_string(),
                Instant::now() - OFFLINE_NODE_RETENTION - Duration::from_secs(1),
            )]))),
            running: Arc::new(AtomicBool::new(true)),
            network: RemoteNetworkConfig {
                port: 0,
                connect: None,
                node_id: None,
                public_endpoint: None,
            },
            current_executable: PathBuf::from("/tmp/waitagent-test"),
        };

        let pruned = prune_expired_offline_nodes(&state, Instant::now());

        assert_eq!(pruned.len(), 1);
        assert_eq!(pruned[0].node_id, "peer-a");

        let records = state
            .records
            .lock()
            .expect("remote runtime owner state mutex should not be poisoned");
        assert_eq!(records.len(), 1);
        assert!(records.contains_key("peer-b\tremote-peer:peer-b:pty9"));
        assert!(state
            .offline_nodes
            .lock()
            .expect("remote runtime owner offline node mutex should not be poisoned")
            .is_empty());
    }

    #[test]
    fn shutdown_command_marks_owner_not_running() {
        let state = RemoteRuntimeOwnerSharedState {
            records: Arc::new(Mutex::new(HashMap::new())),
            offline_nodes: Arc::new(Mutex::new(HashMap::new())),
            running: Arc::new(AtomicBool::new(true)),
            network: RemoteNetworkConfig {
                port: 0,
                connect: None,
                node_id: None,
                public_endpoint: None,
            },
            current_executable: PathBuf::from("/tmp/waitagent-test"),
        };
        let (mut client, mut server) = UnixStream::pair().expect("unix stream pair should open");
        client
            .write_all(
                render_remote_runtime_owner_command(&RemoteRuntimeOwnerCommandEnvelope::Shutdown)
                    .as_bytes(),
            )
            .expect("command should write");
        client
            .shutdown(Shutdown::Write)
            .expect("client shutdown should succeed");

        let response =
            handle_remote_runtime_owner_client(&state, &mut server).expect("command should handle");

        assert_eq!(response.as_deref(), Some("ok\n"));
        assert!(!state.running.load(std::sync::atomic::Ordering::Relaxed));
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

    fn send_command_to_socket(
        socket_path: &std::path::Path,
        command: &RemoteRuntimeOwnerCommandEnvelope,
    ) -> String {
        let mut stream = UnixStream::connect(socket_path).expect("client should connect");
        stream
            .write_all(render_remote_runtime_owner_command(command).as_bytes())
            .expect("command should write");
        stream
            .shutdown(Shutdown::Write)
            .expect("client shutdown should succeed");
        let mut response = String::new();
        stream
            .read_to_string(&mut response)
            .expect("response should read");
        response
    }

    #[test]
    fn remote_runtime_owner_event_loop_prunes_offline_nodes_after_ttl() {
        let network = RemoteNetworkConfig {
            port: 0,
            connect: None,
            node_id: None,
            public_endpoint: None,
        };
        let socket_path = remote_runtime_owner_socket_path(&network);
        let socket_path_for_thread = socket_path.clone();
        let _ = std::fs::remove_file(&socket_path);

        let state = RemoteRuntimeOwnerSharedState {
            records: Arc::new(Mutex::new(HashMap::new())),
            offline_nodes: Arc::new(Mutex::new(HashMap::new())),
            running: Arc::new(AtomicBool::new(true)),
            network: network.clone(),
            current_executable: PathBuf::from("/tmp/waitagent-test"),
        };
        let state_for_thread = state.clone();

        let handle = thread::spawn(move || {
            let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should create");
            runtime
                .block_on(async {
                    let listener = tokio::net::UnixListener::bind(&socket_path_for_thread)
                        .expect("event loop socket should bind");
                    run_remote_runtime_owner_event_loop(listener, state_for_thread).await
                })
                .expect("event loop should run");
        });

        // Wait for the listener to be ready before sending commands.
        thread::sleep(Duration::from_millis(20));

        send_command_to_socket(
            &socket_path,
            &RemoteRuntimeOwnerCommandEnvelope::UpsertSession {
                node_id: "peer-a".to_string(),
                session: remote_session("peer-a", "pty1"),
            },
        );
        send_command_to_socket(
            &socket_path,
            &RemoteRuntimeOwnerCommandEnvelope::MarkNodeOffline {
                node_id: "peer-a".to_string(),
            },
        );

        assert!(
            state
                .offline_nodes
                .lock()
                .expect("offline node mutex should not be poisoned")
                .contains_key("peer-a"),
            "offline node should be recorded"
        );

        // The test build uses a short retention so the TTL arm fires quickly.
        thread::sleep(Duration::from_millis(50));

        assert!(
            state
                .offline_nodes
                .lock()
                .expect("offline node mutex should not be poisoned")
                .is_empty(),
            "TTL arm should prune expired offline node"
        );

        let snapshot =
            send_command_to_socket(&socket_path, &RemoteRuntimeOwnerCommandEnvelope::Snapshot);
        assert!(
            snapshot.starts_with("snapshot"),
            "snapshot should start with header: {snapshot}"
        );
        assert!(
            !snapshot.contains("peer-a"),
            "snapshot should not contain pruned peer: {snapshot}"
        );

        send_command_to_socket(&socket_path, &RemoteRuntimeOwnerCommandEnvelope::Shutdown);

        handle.join().expect("event loop thread should finish");
        let _ = std::fs::remove_file(&socket_path);
    }
}
