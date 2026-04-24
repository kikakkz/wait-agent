use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static SESSION_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WorkspaceInstanceId(String);

impl WorkspaceInstanceId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

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
    pub initial_rows: Option<u16>,
    pub initial_cols: Option<u16>,
}

impl WorkspaceInstanceConfig {
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

pub fn stable_workspace_key(path: &Path) -> String {
    let mut hash = 0xcbf29ce484222325_u64;
    let normalized = path.to_string_lossy();
    for byte in normalized.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::{next_session_key, stable_workspace_key, WorkspaceInstanceConfig};
    use std::path::Path;

    #[test]
    fn stable_workspace_key_is_deterministic() {
        let path = Path::new("/tmp/waitagent/workspace");
        assert_eq!(stable_workspace_key(path), stable_workspace_key(path));
    }

    #[test]
    fn workspace_instance_config_derives_new_tmux_session_identity() {
        let config = WorkspaceInstanceConfig::for_new_session(Path::new("/tmp/waitagent/ws"));

        assert_eq!(config.workspace_dir, Path::new("/tmp/waitagent/ws"));
        assert!(config.workspace_key.len() == 16);
        assert_eq!(config.socket_name, format!("wa-{}", config.workspace_key));
        assert_eq!(config.session_name, config.workspace_key);
        assert_eq!(config.initial_rows, None);
        assert_eq!(config.initial_cols, None);
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
