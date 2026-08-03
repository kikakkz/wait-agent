use crate::domain::session_catalog::ManagedSessionRecord;
use std::error::Error;
use std::fmt;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteSessionCreationRequest {
    pub authority_node_id: String,
    pub cwd_hint: Option<PathBuf>,
    pub cols: usize,
    pub rows: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteSessionCreationError {
    InvalidRequest(String),
    Transport(String),
    Rejected { code: &'static str, message: String },
    Protocol(String),
    Catalog(String),
}

impl fmt::Display for RemoteSessionCreationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRequest(message) => {
                write!(f, "invalid remote session creation request: {message}")
            }
            Self::Transport(message) => {
                write!(f, "remote session creation transport failed: {message}")
            }
            Self::Rejected { code, message } => {
                write!(f, "remote session creation rejected ({code}): {message}")
            }
            Self::Protocol(message) => {
                write!(f, "remote session creation protocol error: {message}")
            }
            Self::Catalog(message) => write!(
                f,
                "remote session creation catalog lookup failed: {message}"
            ),
        }
    }
}

impl Error for RemoteSessionCreationError {}

pub trait SessionCreationPort: Send + Sync {
    fn create_session(
        &self,
        request: RemoteSessionCreationRequest,
    ) -> Result<ManagedSessionRecord, RemoteSessionCreationError>;
}
