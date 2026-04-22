use std::path::{Path, PathBuf};

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
}

impl WorkspaceInstanceConfig {
    pub fn for_workspace_dir(workspace_dir: &Path) -> Self {
        let workspace_key = stable_workspace_key(workspace_dir);
        Self {
            workspace_dir: workspace_dir.to_path_buf(),
            socket_name: format!("wa-{workspace_key}"),
            session_name: format!("waitagent-{workspace_key}"),
            workspace_key,
        }
    }
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
    use super::{stable_workspace_key, WorkspaceInstanceConfig};
    use std::path::Path;

    #[test]
    fn stable_workspace_key_is_deterministic() {
        let path = Path::new("/tmp/waitagent/workspace");
        assert_eq!(stable_workspace_key(path), stable_workspace_key(path));
    }

    #[test]
    fn workspace_instance_config_derives_tmux_identity_from_workspace_dir() {
        let config = WorkspaceInstanceConfig::for_workspace_dir(Path::new("/tmp/waitagent/ws"));

        assert_eq!(config.workspace_dir, Path::new("/tmp/waitagent/ws"));
        assert!(config.workspace_key.len() == 16);
        assert_eq!(config.socket_name, format!("wa-{}", config.workspace_key));
        assert_eq!(
            config.session_name,
            format!("waitagent-{}", config.workspace_key)
        );
    }
}
