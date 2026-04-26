use crate::domain::workspace_paths::WorkspacePaths;
use std::env;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Clone, Copy)]
pub struct WorkspacePathService;

impl WorkspacePathService {
    pub fn new() -> Self {
        Self
    }

    pub fn runtime_root_dir(&self) -> PathBuf {
        env::var("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/tmp"))
            .join("waitagent")
    }

    pub fn resolve_workspace_dir(&self, value: Option<&str>) -> Result<PathBuf, io::Error> {
        let dir = match value {
            Some(path) => PathBuf::from(path),
            None => env::current_dir()?,
        };
        dir.canonicalize()
    }

    pub fn workspace_paths(&self, workspace_dir: &Path) -> WorkspacePaths {
        WorkspacePaths::from_workspace_dir(workspace_dir, &self.runtime_root_dir())
    }
}

#[cfg(test)]
mod tests {
    use super::WorkspacePathService;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn resolve_workspace_dir_canonicalizes_requested_path() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after unix epoch")
            .as_nanos();
        let workspace_dir = std::env::temp_dir().join(format!("waitagent-path-service-{nonce}"));
        fs::create_dir_all(&workspace_dir).expect("workspace dir should exist");

        let resolved = WorkspacePathService::new()
            .resolve_workspace_dir(workspace_dir.to_str())
            .expect("workspace dir should canonicalize");

        assert_eq!(
            resolved,
            workspace_dir
                .canonicalize()
                .expect("workspace dir should canonicalize")
        );
    }

    #[test]
    fn workspace_paths_use_waitagent_runtime_root() {
        let service = WorkspacePathService::new();
        let paths = service.workspace_paths(std::path::Path::new("/tmp/waitagent/ws"));

        assert!(paths.socket_path.to_string_lossy().contains("waitagent"));
    }
}
