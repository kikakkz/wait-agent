use crate::domain::session_catalog::{
    ManagedSessionAddress, ManagedSessionRecord, ManagedSessionTaskState, SessionAvailability,
    SessionTransport,
};
use crate::domain::workspace::WorkspaceSessionRole;
use crate::infra::tmux::TmuxError;
use base64::Engine;
use std::fs;
use std::path::PathBuf;

const SESSION_CATALOG_SNAPSHOT_VERSION: &str = "v1";
const OPTIONAL_NONE_SENTINEL: &str = "~";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionCatalogSnapshotStore {
    path: PathBuf,
}

impl SessionCatalogSnapshotStore {
    pub fn for_socket(socket_name: &str) -> Self {
        Self {
            path: default_session_catalog_snapshot_path(socket_name),
        }
    }

    pub fn load(&self) -> Result<Option<Vec<ManagedSessionRecord>>, TmuxError> {
        if !self.path.exists() {
            return Ok(None);
        }
        let contents = fs::read_to_string(&self.path).map_err(|error| {
            TmuxError::new(format!(
                "failed to read session catalog snapshot {}: {error}",
                self.path.display()
            ))
        })?;
        let sessions = contents
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(parse_session_snapshot_record)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Some(sessions))
    }

    pub fn store(&self, sessions: &[ManagedSessionRecord]) -> Result<(), TmuxError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                TmuxError::new(format!(
                    "failed to create session catalog snapshot directory {}: {error}",
                    parent.display()
                ))
            })?;
        }
        let mut contents = sessions
            .iter()
            .map(render_session_snapshot_record)
            .collect::<Vec<_>>()
            .join("\n");
        if !contents.is_empty() {
            contents.push('\n');
        }
        let tmp_path = self
            .path
            .with_extension(format!("tmp-{}", std::process::id()));
        fs::write(&tmp_path, contents).map_err(|error| {
            TmuxError::new(format!(
                "failed to write session catalog snapshot {}: {error}",
                tmp_path.display()
            ))
        })?;
        fs::rename(&tmp_path, &self.path).map_err(|error| {
            let _ = fs::remove_file(&tmp_path);
            TmuxError::new(format!(
                "failed to replace session catalog snapshot {}: {error}",
                self.path.display()
            ))
        })
    }

    pub fn remove(&self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn default_session_catalog_snapshot_path(socket_name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "waitagent-session-catalog-{}.tsv",
        sanitize_path_component(socket_name)
    ))
}

fn render_session_snapshot_record(session: &ManagedSessionRecord) -> String {
    let workspace_dir = session
        .workspace_dir
        .as_ref()
        .map(|path| path.to_string_lossy().into_owned());
    let current_path = session
        .current_path
        .as_ref()
        .map(|path| path.to_string_lossy().into_owned());
    [
        SESSION_CATALOG_SNAPSHOT_VERSION.to_string(),
        session
            .address
            .transport()
            .stable_snapshot_label()
            .to_string(),
        encode_string_field(session.address.authority_id()),
        encode_string_field(session.address.session_id()),
        encode_optional_string_field(session.selector.as_deref()),
        session.availability.as_str().to_string(),
        encode_optional_string_field(workspace_dir.as_deref()),
        encode_optional_string_field(session.workspace_key.as_deref()),
        encode_optional_string_field(session.session_role.map(WorkspaceSessionRole::as_str)),
        session.attached_clients.to_string(),
        session.window_count.to_string(),
        encode_optional_string_field(session.command_name.as_deref()),
        encode_optional_string_field(current_path.as_deref()),
        session.task_state.as_str().to_string(),
    ]
    .join("\t")
}

fn parse_session_snapshot_record(line: &str) -> Result<ManagedSessionRecord, TmuxError> {
    let parts = line.split('\t').collect::<Vec<_>>();
    if parts.first().copied() != Some(SESSION_CATALOG_SNAPSHOT_VERSION) {
        return Err(TmuxError::new(format!(
            "unsupported session catalog snapshot record version `{}`",
            parts.first().copied().unwrap_or_default()
        )));
    }
    if parts.len() != 14 {
        return Err(TmuxError::new(format!(
            "session catalog snapshot record must contain 14 tab-separated fields, got {}",
            parts.len()
        )));
    }
    let transport = SessionTransport::parse_snapshot_label(parts[1]).ok_or_else(|| {
        TmuxError::new(format!(
            "unsupported session catalog snapshot transport `{}`",
            parts[1]
        ))
    })?;
    let authority_id = decode_string_field(parts[2])?;
    let session_id = decode_string_field(parts[3])?;
    let selector = decode_optional_string_field(parts[4])?;
    let availability = SessionAvailability::parse(parts[5]).ok_or_else(|| {
        TmuxError::new(format!(
            "unsupported session catalog snapshot availability `{}`",
            parts[5]
        ))
    })?;
    let workspace_dir = decode_optional_string_field(parts[6])?.map(PathBuf::from);
    let workspace_key = decode_optional_string_field(parts[7])?;
    let session_role = decode_optional_string_field(parts[8])?
        .as_deref()
        .and_then(WorkspaceSessionRole::parse);
    let attached_clients = parts[9].parse::<usize>().map_err(|error| {
        TmuxError::new(format!(
            "invalid session catalog snapshot attached client count `{}`: {error}",
            parts[9]
        ))
    })?;
    let window_count = parts[10].parse::<usize>().map_err(|error| {
        TmuxError::new(format!(
            "invalid session catalog snapshot window count `{}`: {error}",
            parts[10]
        ))
    })?;
    let command_name = decode_optional_string_field(parts[11])?;
    let current_path = decode_optional_string_field(parts[12])?.map(PathBuf::from);
    let task_state = ManagedSessionTaskState::parse(parts[13]).ok_or_else(|| {
        TmuxError::new(format!(
            "unsupported session catalog snapshot task state `{}`",
            parts[13]
        ))
    })?;

    let address = match transport {
        SessionTransport::LocalTmux => ManagedSessionAddress::local_tmux(authority_id, session_id),
        SessionTransport::RemotePeer => {
            ManagedSessionAddress::remote_peer(authority_id, session_id)
        }
    };
    Ok(ManagedSessionRecord {
        address,
        selector,
        availability,
        workspace_dir,
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

trait SessionTransportSnapshotExt {
    fn stable_snapshot_label(&self) -> &'static str;
    fn parse_snapshot_label(value: &str) -> Option<Self>
    where
        Self: Sized;
}

impl SessionTransportSnapshotExt for SessionTransport {
    fn stable_snapshot_label(&self) -> &'static str {
        match self {
            Self::LocalTmux => "local-tmux",
            Self::RemotePeer => "remote-peer",
        }
    }

    fn parse_snapshot_label(value: &str) -> Option<Self> {
        match value {
            "local-tmux" => Some(Self::LocalTmux),
            "remote-peer" => Some(Self::RemotePeer),
            _ => None,
        }
    }
}

fn encode_string_field(value: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(value.as_bytes())
}

fn decode_string_field(value: &str) -> Result<String, TmuxError> {
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(value)
        .map_err(|error| TmuxError::new(format!("invalid base64 field `{value}`: {error}")))?;
    String::from_utf8(decoded).map_err(|error| {
        TmuxError::new(format!(
            "session catalog snapshot field is not valid UTF-8: {error}"
        ))
    })
}

fn encode_optional_string_field(value: Option<&str>) -> String {
    value
        .map(encode_string_field)
        .unwrap_or_else(|| OPTIONAL_NONE_SENTINEL.to_string())
}

fn decode_optional_string_field(value: &str) -> Result<Option<String>, TmuxError> {
    if value == OPTIONAL_NONE_SENTINEL {
        return Ok(None);
    }
    decode_string_field(value).map(Some)
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
