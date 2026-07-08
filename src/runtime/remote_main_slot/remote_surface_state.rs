use crate::domain::session_catalog::ManagedSessionRecord;
use crate::infra::tmux::{EmbeddedTmuxBackend, TmuxPaneId, TmuxSocketName, TmuxWorkspaceHandle};
use crate::lifecycle::LifecycleError;
use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) const WAITAGENT_REMOTE_SURFACE_TARGET_OPTION: &str = "@waitagent_remote_surface_target";
pub(crate) const WAITAGENT_REMOTE_SURFACE_AUTHORITY_OPTION: &str =
    "@waitagent_remote_surface_authority";
pub(crate) const WAITAGENT_REMOTE_SURFACE_STATE_OPTION: &str = "@waitagent_remote_surface_state";
pub(crate) const WAITAGENT_REMOTE_SURFACE_GENERATION_OPTION: &str =
    "@waitagent_remote_surface_generation";
pub(crate) const WAITAGENT_REMOTE_SURFACE_UPDATED_AT_OPTION: &str =
    "@waitagent_remote_surface_updated_at";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RemoteSurfaceState {
    Starting,
    Connected,
    Reconnecting,
    Exited,
}

impl RemoteSurfaceState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Connected => "connected",
            Self::Reconnecting => "reconnecting",
            Self::Exited => "exited",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "starting" => Some(Self::Starting),
            "connected" => Some(Self::Connected),
            "reconnecting" => Some(Self::Reconnecting),
            "exited" => Some(Self::Exited),
            _ => None,
        }
    }
}

pub(crate) fn mark_remote_surface_state_from_env(
    backend: &EmbeddedTmuxBackend,
    socket_name: &str,
    target: &ManagedSessionRecord,
    generation: Option<u64>,
    state: RemoteSurfaceState,
) -> Result<(), LifecycleError> {
    let Ok(pane_id) = std::env::var("TMUX_PANE") else {
        return Ok(());
    };
    let pane_id = pane_id.trim();
    if pane_id.is_empty() {
        return Ok(());
    }
    mark_remote_surface_state_on_pane(
        backend,
        &TmuxSocketName::new(socket_name),
        &TmuxPaneId::new(pane_id),
        target,
        generation,
        state,
    )
}

pub(crate) fn mark_remote_surface_state_on_pane(
    backend: &EmbeddedTmuxBackend,
    socket_name: &TmuxSocketName,
    pane: &TmuxPaneId,
    target: &ManagedSessionRecord,
    generation: Option<u64>,
    state: RemoteSurfaceState,
) -> Result<(), LifecycleError> {
    set_pane_option(
        backend,
        socket_name,
        pane,
        WAITAGENT_REMOTE_SURFACE_TARGET_OPTION,
        target.address.qualified_target().as_str(),
    )?;
    set_pane_option(
        backend,
        socket_name,
        pane,
        WAITAGENT_REMOTE_SURFACE_AUTHORITY_OPTION,
        target.address.authority_id(),
    )?;
    set_pane_option(
        backend,
        socket_name,
        pane,
        WAITAGENT_REMOTE_SURFACE_STATE_OPTION,
        state.as_str(),
    )?;
    if let Some(generation) = generation {
        set_pane_option(
            backend,
            socket_name,
            pane,
            WAITAGENT_REMOTE_SURFACE_GENERATION_OPTION,
            &generation.to_string(),
        )?;
    }
    set_pane_option(
        backend,
        socket_name,
        pane,
        WAITAGENT_REMOTE_SURFACE_UPDATED_AT_OPTION,
        &unix_epoch_millis().to_string(),
    )?;
    Ok(())
}

pub(crate) fn remote_surface_pane_is_reusable(
    backend: &EmbeddedTmuxBackend,
    workspace: &TmuxWorkspaceHandle,
    pane: &TmuxPaneId,
    target: &ManagedSessionRecord,
) -> Result<bool, LifecycleError> {
    let socket_name = &workspace.socket_name;
    let expected_target = target.address.qualified_target();
    let target_id = backend
        .show_pane_option_on_socket(socket_name, pane, WAITAGENT_REMOTE_SURFACE_TARGET_OPTION)
        .map_err(remote_surface_error)?;
    let authority_id = backend
        .show_pane_option_on_socket(socket_name, pane, WAITAGENT_REMOTE_SURFACE_AUTHORITY_OPTION)
        .map_err(remote_surface_error)?;
    let state = backend
        .show_pane_option_on_socket(socket_name, pane, WAITAGENT_REMOTE_SURFACE_STATE_OPTION)
        .map_err(remote_surface_error)?
        .and_then(|value| RemoteSurfaceState::parse(&value));

    // If the pane predates remote-surface tracking, fall back to the legacy
    // reuse rule and let tmux liveness checks decide.
    if target_id.is_none() && authority_id.is_none() && state.is_none() {
        return Ok(true);
    }

    if target_id.as_deref() != Some(expected_target.as_str()) {
        return Ok(false);
    }
    if authority_id.as_deref() != Some(target.address.authority_id()) {
        return Ok(false);
    }

    // A pane that explicitly reached the Exited state is stale and must not
    // be reused. Starting, connected, and reconnecting panes are all live
    // enough to keep, because the pane runtime owns their lifecycle.
    Ok(!matches!(state, Some(RemoteSurfaceState::Exited)))
}

fn set_pane_option(
    backend: &EmbeddedTmuxBackend,
    socket_name: &TmuxSocketName,
    pane: &TmuxPaneId,
    option: &str,
    value: &str,
) -> Result<(), LifecycleError> {
    backend
        .set_pane_option_on_socket(socket_name, pane, option, value)
        .map_err(remote_surface_error)
}

fn unix_epoch_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}

fn remote_surface_error(error: crate::infra::tmux::TmuxError) -> LifecycleError {
    LifecycleError::Io(
        "tmux-native remote surface state command failed".to_string(),
        std::io::Error::new(std::io::ErrorKind::Other, error.to_string()),
    )
}
