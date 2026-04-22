use crate::domain::workspace::stable_workspace_key;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspacePaths {
    pub workspace_dir: PathBuf,
    pub socket_path: PathBuf,
}

impl WorkspacePaths {
    pub fn from_workspace_dir(workspace_dir: &Path, runtime_root_dir: &Path) -> Self {
        let workspace_key = stable_workspace_key(workspace_dir);
        let socket_path = runtime_root_dir.join(format!("{workspace_key}.sock"));
        Self {
            workspace_dir: workspace_dir.to_path_buf(),
            socket_path,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::WorkspacePaths;
    use std::path::Path;

    #[test]
    fn workspace_paths_derive_socket_path_from_workspace_dir() {
        let paths = WorkspacePaths::from_workspace_dir(
            Path::new("/tmp/waitagent/ws"),
            Path::new("/tmp/waitagent-runtime"),
        );

        assert_eq!(paths.workspace_dir, Path::new("/tmp/waitagent/ws"));
        assert!(paths.socket_path.starts_with("/tmp/waitagent-runtime"));
        assert!(paths.socket_path.to_string_lossy().ends_with(".sock"));
    }
}
