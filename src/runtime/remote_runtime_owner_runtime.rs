use crate::cli::{prepend_global_network_args, RemoteNetworkConfig, RemoteRuntimeOwnerCommand};
use crate::domain::session_catalog::{
    ManagedSessionAddress, ManagedSessionRecord, ManagedSessionTaskState, SessionAvailability,
};
use crate::domain::workspace::WorkspaceSessionRole;
use crate::infra::tmux::{tmux_socket_dir, EmbeddedTmuxBackend, TmuxSocketName};
use crate::lifecycle::LifecycleError;
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
use std::time::Duration;

const REMOTE_RUNTIME_OWNER_READY_RETRIES: usize = 20;
const REMOTE_RUNTIME_OWNER_READY_SLEEP: Duration = Duration::from_millis(25);
const REMOTE_RUNTIME_OWNER_IDLE_POLL_SLEEP: Duration = Duration::from_millis(100);

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
    pub fn from_build_env() -> Result<Self, LifecycleError> {
        Self::from_build_env_with_network(RemoteNetworkConfig::default())
    }

    pub fn from_build_env_with_network(
        network: RemoteNetworkConfig,
    ) -> Result<Self, LifecycleError> {
        Ok(Self {
            current_executable: std::env::current_exe().map_err(|error| {
                LifecycleError::Io(
                    "failed to locate current waitagent executable".to_string(),
                    error,
                )
            })?,
            network,
        })
    }

    #[cfg(test)]
    pub fn new_for_tests(current_executable: PathBuf, network: RemoteNetworkConfig) -> Self {
        Self {
            current_executable,
            network,
        }
    }

    pub fn run_owner(&self, command: RemoteRuntimeOwnerCommand) -> Result<(), LifecycleError> {
        let socket_path = remote_runtime_owner_socket_path(&command.socket_name);
        if socket_path.exists() {
            let _ = fs::remove_file(&socket_path);
        }
        let listener = UnixListener::bind(&socket_path).map_err(remote_runtime_owner_error)?;
        listener
            .set_nonblocking(true)
            .map_err(remote_runtime_owner_error)?;
        let state = RemoteRuntimeOwnerSharedState {
            records: Arc::new(Mutex::new(HashMap::new())),
            running: Arc::new(AtomicBool::new(true)),
        };
        while state.running.load(Ordering::Relaxed) {
            if !backend_socket_still_exists(&command.socket_name) {
                break;
            }
            let accepted = match listener.accept() {
                Ok((stream, _)) => Ok(stream),
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    thread::sleep(REMOTE_RUNTIME_OWNER_IDLE_POLL_SLEEP);
                    continue;
                }
                Err(error) => Err(error),
            };
            if !state.running.load(Ordering::Relaxed) {
                break;
            }
            let Ok(mut stream) = accepted.map_err(remote_runtime_owner_error) else {
                break;
            };
            let response = handle_remote_runtime_owner_client(&state, &mut stream);
            match response {
                Ok(Some(payload)) => {
                    let _ = stream.write_all(payload.as_bytes());
                    let _ = stream.flush();
                }
                Ok(None) => {}
                Err(_) => {}
            }
        }
        let _ = fs::remove_file(&socket_path);
        Ok(())
    }

    pub fn ensure_owner_running(&self, socket_name: &str) -> Result<(), LifecycleError> {
        ensure_remote_runtime_owner_process_running(
            &self.current_executable,
            socket_name,
            &self.network,
        )
    }

    pub fn upsert_session(
        &self,
        socket_name: &str,
        node_id: &str,
        session: &ManagedSessionRecord,
    ) -> Result<(), LifecycleError> {
        self.ensure_owner_running(socket_name)?;
        signal_remote_runtime_owner_command(
            socket_name,
            RemoteRuntimeOwnerCommandEnvelope::UpsertSession {
                node_id: node_id.to_string(),
                session: session.clone(),
            },
        )
    }

    pub fn remove_session(
        &self,
        socket_name: &str,
        node_id: &str,
        authority_id: &str,
        transport_session_id: &str,
    ) -> Result<(), LifecycleError> {
        self.ensure_owner_running(socket_name)?;
        signal_remote_runtime_owner_command(
            socket_name,
            RemoteRuntimeOwnerCommandEnvelope::RemoveSession {
                node_id: node_id.to_string(),
                authority_id: authority_id.to_string(),
                transport_session_id: transport_session_id.to_string(),
            },
        )
    }

    pub fn remove_node(&self, socket_name: &str, node_id: &str) -> Result<(), LifecycleError> {
        self.ensure_owner_running(socket_name)?;
        signal_remote_runtime_owner_command(
            socket_name,
            RemoteRuntimeOwnerCommandEnvelope::RemoveNode {
                node_id: node_id.to_string(),
            },
        )
    }

    pub fn snapshot(
        &self,
        socket_name: &str,
    ) -> Result<RemoteRuntimeOwnerSnapshot, LifecycleError> {
        self.ensure_owner_running(socket_name)?;
        let mut stream = UnixStream::connect(remote_runtime_owner_socket_path(socket_name))
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

    pub fn try_snapshot(
        &self,
        socket_name: &str,
    ) -> Result<RemoteRuntimeOwnerSnapshot, LifecycleError> {
        let socket_path = remote_runtime_owner_socket_path(socket_name);
        if !socket_path.exists() {
            return Ok(RemoteRuntimeOwnerSnapshot {
                sessions: Vec::new(),
            });
        }
        let mut stream = match UnixStream::connect(&socket_path) {
            Ok(stream) => stream,
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
}

fn handle_remote_runtime_owner_client(
    state: &RemoteRuntimeOwnerSharedState,
    stream: &mut UnixStream,
) -> Result<Option<String>, LifecycleError> {
    let command = read_remote_runtime_owner_command(stream)?;
    match command {
        RemoteRuntimeOwnerCommandEnvelope::UpsertSession { node_id, session } => {
            let key = owned_record_key(&node_id, session.address.id().as_str());
            state
                .records
                .lock()
                .expect("remote runtime owner state mutex should not be poisoned")
                .insert(key, OwnerStateRecord { node_id, session });
            Ok(None)
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
            Ok(None)
        }
        RemoteRuntimeOwnerCommandEnvelope::RemoveNode { node_id } => {
            let mut guard = state
                .records
                .lock()
                .expect("remote runtime owner state mutex should not be poisoned");
            guard.retain(|_, record| record.node_id != node_id);
            Ok(None)
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
    }
}

pub(crate) fn ensure_remote_runtime_owner_process_running(
    current_executable: &Path,
    socket_name: &str,
    network: &RemoteNetworkConfig,
) -> Result<(), LifecycleError> {
    let socket_path = remote_runtime_owner_socket_path(socket_name);
    if remote_runtime_owner_available(&socket_path) {
        return Ok(());
    }
    if socket_path.exists() {
        let _ = fs::remove_file(&socket_path);
    }

    spawn_waitagent_sidecar(
        current_executable,
        remote_runtime_owner_args(socket_name, network),
    )
    .map_err(remote_runtime_owner_error)?;

    for _ in 0..REMOTE_RUNTIME_OWNER_READY_RETRIES {
        if remote_runtime_owner_available(&socket_path) {
            return Ok(());
        }
        thread::sleep(REMOTE_RUNTIME_OWNER_READY_SLEEP);
    }

    Err(LifecycleError::Protocol(format!(
        "remote runtime owner for socket `{socket_name}` did not become ready"
    )))
}

fn remote_runtime_owner_available(socket_path: &Path) -> bool {
    UnixStream::connect(socket_path).is_ok()
}

fn backend_socket_still_exists(socket_name: &str) -> bool {
    let socket_path = tmux_socket_dir().join(socket_name);
    if !socket_path.exists() {
        return false;
    }
    let Ok(backend) = EmbeddedTmuxBackend::from_build_env() else {
        return false;
    };
    backend.socket_is_live(&TmuxSocketName::new(socket_name))
}

pub(crate) fn remote_runtime_owner_socket_path(socket_name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "waitagent-remote-runtime-owner-{}.sock",
        sanitize_path_component(socket_name)
    ))
}

pub(crate) fn remote_runtime_owner_args(
    socket_name: &str,
    network: &RemoteNetworkConfig,
) -> Vec<String> {
    prepend_global_network_args(
        vec![
            "__remote-runtime-owner".to_string(),
            "--socket-name".to_string(),
            socket_name.to_string(),
        ],
        network,
    )
}

fn signal_remote_runtime_owner_command(
    socket_name: &str,
    command: RemoteRuntimeOwnerCommandEnvelope,
) -> Result<(), LifecycleError> {
    let mut stream = UnixStream::connect(remote_runtime_owner_socket_path(socket_name))
        .map_err(remote_runtime_owner_error)?;
    stream
        .write_all(render_remote_runtime_owner_command(&command).as_bytes())
        .map_err(remote_runtime_owner_error)?;
    stream.flush().map_err(remote_runtime_owner_error)?;
    stream
        .shutdown(Shutdown::Write)
        .map_err(remote_runtime_owner_error)
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
        backend_socket_still_exists, handle_remote_runtime_owner_client,
        parse_remote_runtime_owner_command, parse_remote_runtime_owner_snapshot,
        remote_runtime_owner_args, remote_runtime_owner_socket_path,
        render_remote_runtime_owner_command, render_remote_runtime_owner_snapshot,
        OwnerStateRecord, RemoteRuntimeOwnerCommandEnvelope, RemoteRuntimeOwnerSharedState,
        RemoteRuntimeOwnerSnapshot,
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
        };

        let args = remote_runtime_owner_args("wa-1", &network);

        assert_eq!(
            args,
            vec![
                "--port",
                "9001",
                "--connect",
                "10.0.0.8:7474",
                "__remote-runtime-owner",
                "--socket-name",
                "wa-1",
            ]
        );
    }

    #[test]
    fn remote_runtime_owner_socket_path_is_scoped_to_socket_name() {
        let path = remote_runtime_owner_socket_path("wa/local");

        assert_eq!(
            path.file_name().and_then(|value| value.to_str()),
            Some("waitagent-remote-runtime-owner-wa_local.sock")
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

        assert!(response.is_none());
        let records = state
            .records
            .lock()
            .expect("remote runtime owner state mutex should not be poisoned");
        assert_eq!(records.len(), 1);
        assert!(records.contains_key("peer-b\tremote-peer:peer-b:pty9"));
    }

    #[test]
    fn remote_runtime_owner_socket_path_lives_in_tmp() {
        let path = remote_runtime_owner_socket_path("wa-test");

        assert_eq!(path.parent(), Some(Path::new("/tmp")));
    }

    #[test]
    fn backend_socket_presence_uses_tmux_socket_dir() {
        let socket_name = format!("wa-owner-test-{}", std::process::id());
        let socket_path = crate::infra::tmux::tmux_socket_dir().join(&socket_name);
        let _ = std::fs::remove_file(&socket_path);

        assert!(!backend_socket_still_exists(&socket_name));

        std::fs::write(&socket_path, b"stub").expect("socket marker should be writable");
        assert!(!backend_socket_still_exists(&socket_name));

        std::fs::remove_file(&socket_path).expect("socket marker should clean up");
    }
}
