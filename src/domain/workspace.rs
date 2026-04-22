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
    pub workspace_key: String,
    pub socket_name: String,
    pub session_name: String,
}
