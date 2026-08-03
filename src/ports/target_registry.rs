use crate::domain::session_catalog::ManagedSessionRecord;
use std::error::Error;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetRegistryError {
    message: String,
}

impl TargetRegistryError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for TargetRegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl Error for TargetRegistryError {}

pub trait TargetCatalogGateway {
    type Error;

    fn list_targets(&self) -> Result<Vec<ManagedSessionRecord>, Self::Error>;
}

pub trait TargetRegistryPort: Send + Sync {
    fn list_targets(&self) -> Result<Vec<ManagedSessionRecord>, TargetRegistryError>;
    fn list_targets_on_authority(
        &self,
        authority_id: &str,
    ) -> Result<Vec<ManagedSessionRecord>, TargetRegistryError>;
}
