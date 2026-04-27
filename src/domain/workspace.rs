use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static SESSION_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WorkspaceSessionRole {
    WorkspaceChrome,
    TargetHost,
}

impl WorkspaceSessionRole {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::WorkspaceChrome => "workspace-chrome",
            Self::TargetHost => "target-host",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "workspace-chrome" => Some(Self::WorkspaceChrome),
            "target-host" => Some(Self::TargetHost),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WorkspaceInstanceId(String);

impl WorkspaceInstanceId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    #[cfg(test)]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceInstanceConfig {
    pub workspace_dir: PathBuf,
    pub workspace_key: String,
    pub socket_name: String,
    pub session_name: String,
    pub session_role: WorkspaceSessionRole,
    pub initial_rows: Option<u16>,
    pub initial_cols: Option<u16>,
}

impl WorkspaceInstanceConfig {
    #[cfg(test)]
    pub fn for_new_session(workspace_dir: &Path) -> Self {
        Self::for_new_session_with_size(workspace_dir, None, None)
    }

    pub fn for_new_session_with_size(
        workspace_dir: &Path,
        rows: Option<u16>,
        cols: Option<u16>,
    ) -> Self {
        let workspace_key = next_session_key();
        Self {
            workspace_dir: workspace_dir.to_path_buf(),
            socket_name: format!("wa-{workspace_key}"),
            session_name: workspace_key.clone(),
            workspace_key,
            session_role: WorkspaceSessionRole::WorkspaceChrome,
            initial_rows: rows,
            initial_cols: cols,
        }
    }

    pub fn for_new_target_on_socket_with_size(
        workspace_dir: &Path,
        socket_name: impl Into<String>,
        rows: Option<u16>,
        cols: Option<u16>,
    ) -> Self {
        let workspace_key = next_session_key();
        Self {
            workspace_dir: workspace_dir.to_path_buf(),
            socket_name: socket_name.into(),
            session_name: workspace_key.clone(),
            workspace_key,
            session_role: WorkspaceSessionRole::TargetHost,
            initial_rows: rows,
            initial_cols: cols,
        }
    }
}

pub fn next_session_key() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let counter = u128::from(SESSION_COUNTER.fetch_add(1, Ordering::Relaxed));
    let pid = u128::from(std::process::id());
    let mixed = nanos ^ (counter << 17) ^ (pid << 49);
    let lower = (mixed & u128::from(u64::MAX)) as u64;
    format!("{lower:016x}")
}

#[cfg(test)]
mod tests {
    use super::{next_session_key, WorkspaceInstanceConfig, WorkspaceSessionRole};
    use std::path::Path;

    #[test]
    fn workspace_instance_config_derives_new_tmux_session_identity() {
        let config = WorkspaceInstanceConfig::for_new_session(Path::new("/tmp/waitagent/ws"));

        assert_eq!(config.workspace_dir, Path::new("/tmp/waitagent/ws"));
        assert!(config.workspace_key.len() == 16);
        assert_eq!(config.socket_name, format!("wa-{}", config.workspace_key));
        assert_eq!(config.session_name, config.workspace_key);
        assert_eq!(config.session_role, WorkspaceSessionRole::WorkspaceChrome);
        assert_eq!(config.initial_rows, None);
        assert_eq!(config.initial_cols, None);
    }

    #[test]
    fn workspace_instance_config_can_reuse_an_existing_socket_for_new_target_sessions() {
        let config = WorkspaceInstanceConfig::for_new_target_on_socket_with_size(
            Path::new("/tmp/waitagent/ws"),
            "wa-existing",
            Some(40),
            Some(120),
        );

        assert_eq!(config.workspace_dir, Path::new("/tmp/waitagent/ws"));
        assert_eq!(config.socket_name, "wa-existing");
        assert_eq!(config.session_name, config.workspace_key);
        assert_eq!(config.session_role, WorkspaceSessionRole::TargetHost);
        assert_eq!(config.initial_rows, Some(40));
        assert_eq!(config.initial_cols, Some(120));
    }

    #[test]
    fn workspace_session_role_round_trips_through_stable_labels() {
        assert_eq!(
            WorkspaceSessionRole::parse(WorkspaceSessionRole::WorkspaceChrome.as_str()),
            Some(WorkspaceSessionRole::WorkspaceChrome)
        );
        assert_eq!(
            WorkspaceSessionRole::parse(WorkspaceSessionRole::TargetHost.as_str()),
            Some(WorkspaceSessionRole::TargetHost)
        );
        assert_eq!(WorkspaceSessionRole::parse("unknown"), None);
    }

    #[test]
    fn next_session_key_returns_fixed_width_hex_identity() {
        let first = next_session_key();
        let second = next_session_key();

        assert_eq!(first.len(), 16);
        assert_eq!(second.len(), 16);
        assert_ne!(first, second);
    }
}
