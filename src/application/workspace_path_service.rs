use std::env;
use std::io;
use std::path::PathBuf;

#[derive(Debug, Default, Clone, Copy)]
pub struct WorkspacePathService;

impl WorkspacePathService {
    pub fn new() -> Self {
        Self
    }

    pub fn resolve_workspace_dir(&self, value: Option<&str>) -> Result<PathBuf, io::Error> {
        let dir = match value {
            Some(path) => PathBuf::from(path),
            None => env::current_dir()?,
        };
        dir.canonicalize()
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
    fn resolve_workspace_dir_defaults_to_current_directory() {
        let expected = std::env::current_dir()
            .expect("current dir should resolve")
            .canonicalize()
            .expect("current dir should canonicalize");

        let resolved = WorkspacePathService::new()
            .resolve_workspace_dir(None)
            .expect("current workspace dir should resolve");

        assert_eq!(resolved, expected);
    }
}
