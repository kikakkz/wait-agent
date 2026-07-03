use crate::domain::session_catalog::ManagedSessionTaskState;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSignalEnvelope {
    pub version: u32,
    pub agent: String,
    pub event: String,
    pub socket: String,
    pub session: String,
    pub pane: String,
    pub token: String,
    #[serde(default)]
    pub payload: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentStateEffect {
    Set(ManagedSessionTaskState),
    Clear,
}
