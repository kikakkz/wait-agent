//! Preserved tmux compatibility layer used by the remote runtime.
//!
//! The actual tmux backend implementation was removed in Phase 4. The types and
//! stub backend below remain only to keep the ratatui remote path compiling and
//! will be refactored away in a later phase.
// Preserved for ratatui remote path; tmux dependency to be removed in a later phase.
pub(crate) use crate::infra::tmux_backend::EmbeddedTmuxBackend;
pub(crate) use crate::infra::tmux_error::tmux_socket_dir;
pub use crate::infra::tmux_error::TmuxError;
#[allow(unused_imports)]
pub use crate::infra::tmux_types::{
    RemoteTargetPublicationBinding, TmuxChromeGateway, TmuxControlGateway, TmuxGateway,
    TmuxLayoutGateway, TmuxPaneId, TmuxPaneInfo, TmuxProgram, TmuxSessionGateway, TmuxSessionName,
    TmuxSocketName, TmuxSplitSize, TmuxWindowHandle, TmuxWindowId, TmuxWorkspaceHandle,
};

pub(crate) const WAITAGENT_AGENT_SIGNAL_AGENT_OPTION: &str = "@waitagent_agent_signal_agent";
pub(crate) const WAITAGENT_AGENT_SIGNAL_PANE_OPTION: &str = "@waitagent_agent_signal_pane";
pub(crate) const WAITAGENT_AGENT_SIGNAL_STATE_OPTION: &str = "@waitagent_agent_signal_state";
pub(crate) const WAITAGENT_AGENT_SIGNAL_TOKEN_OPTION: &str = "@waitagent_agent_signal_token";
pub(crate) const WAITAGENT_AGENT_SIGNAL_UPDATED_AT_OPTION: &str =
    "@waitagent_agent_signal_updated_at";
pub(crate) const WAITAGENT_GEOMETRY_APPLYING_OPTION: &str = "@waitagent_geometry_applying";
pub(crate) const WAITAGENT_PANE_ROLE_CONTENT: &str = "content";
pub(crate) const WAITAGENT_PANE_ROLE_OPTION: &str = "@waitagent_pane_role";
pub(crate) const WAITAGENT_PANE_SESSION_INSTANCE_OPTION: &str = "@waitagent_session_instance_id";
pub(crate) const WAITAGENT_PANE_TARGET_ID_OPTION: &str = "@waitagent_target_id";
pub(crate) const WAITAGENT_PANE_TARGET_SESSION_OPTION: &str = "@waitagent_target_session_name";
pub(crate) const WAITAGENT_REMOTE_PUBLICATION_AUTHORITY_ID_ENV: &str =
    "WAITAGENT_REMOTE_PUBLICATION_AUTHORITY_ID";
pub(crate) const WAITAGENT_REMOTE_PUBLICATION_SELECTOR_ENV: &str =
    "WAITAGENT_REMOTE_PUBLICATION_SELECTOR";
pub(crate) const WAITAGENT_REMOTE_PUBLICATION_TRANSPORT_SESSION_ID_ENV: &str =
    "WAITAGENT_REMOTE_PUBLICATION_TRANSPORT_SESSION_ID";
pub(crate) const WAITAGENT_RUNTIME_COMMAND_OVERRIDE_OPTION: &str =
    "@waitagent_runtime_command_override";
pub(crate) const WAITAGENT_RUNTIME_RUNNING_OVERRIDE: &str = "__waitagent_running__";
