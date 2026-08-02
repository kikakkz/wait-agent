//! Stub tmux backend preserved for remote runtime compatibility.
//!
//! The real vendored tmux backend was removed in Phase 4. This module provides
//! enough of the `EmbeddedTmuxBackend` API surface to keep the ratatui remote
//! path compiling. Calls that would have driven tmux now return errors; they are
//! unreachable in the ratatui-only default path and will be removed in a later
//! phase.
use crate::domain::session_catalog::{ManagedSessionAddress, ManagedSessionRecord};
use crate::domain::workspace::{WorkspaceInstanceConfig, WorkspaceSessionRole};
use crate::infra::tmux_error::TmuxError;
use crate::infra::tmux_types::{
    RemoteTargetPublicationBinding, TmuxChromeGateway, TmuxControlGateway, TmuxGateway,
    TmuxLayoutGateway, TmuxPaneId, TmuxPaneInfo, TmuxProgram, TmuxSessionGateway, TmuxSessionName,
    TmuxSocketName, TmuxSplitSize, TmuxWindowHandle, TmuxWorkspaceHandle,
};
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct ChromeRefreshEvent {
    pub socket_name: String,
    pub session_name: String,
    pub pane_id: String,
    pub pane_generation: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaitagentSessionListEntry {
    pub socket_name: String,
    pub session_name: String,
    pub attached_clients: usize,
    pub window_count: usize,
    pub created_at_unix_secs: Option<u64>,
    pub session_role: Option<WorkspaceSessionRole>,
}

impl WaitagentSessionListEntry {
    pub fn display_session_id(&self) -> &str {
        self.session_name
            .strip_prefix("waitagent-")
            .unwrap_or(self.session_name.as_str())
    }

    pub fn role_tag(&self) -> &'static str {
        match self.session_role {
            Some(WorkspaceSessionRole::WorkspaceChrome) => " [main]",
            Some(WorkspaceSessionRole::TargetHost) => " [target]",
            _ => "",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WaitagentSocketCleanupReport {
    pub live: usize,
    pub removed: usize,
}

fn unsupported() -> TmuxError {
    TmuxError::new("tmux backend is no longer available in the ratatui-only build")
}

#[derive(Debug, Clone)]
pub struct EmbeddedTmuxBackend;

impl EmbeddedTmuxBackend {
    pub fn from_build_env() -> Result<Self, TmuxError> {
        Err(unsupported())
    }

    pub fn from_build_env_with_source(_source: VendoredTmuxSource) -> Result<Self, TmuxError> {
        Err(unsupported())
    }

    pub fn source(&self) -> &VendoredTmuxSource {
        panic!("tmux backend is no longer available")
    }

    pub fn build_status(&self) -> &TmuxGlueBuildStatus {
        panic!("tmux backend is no longer available")
    }

    pub fn socket_is_live(&self, _socket_name: &TmuxSocketName) -> bool {
        false
    }

    pub fn discover_waitagent_sockets(&self) -> Result<Vec<TmuxSocketName>, TmuxError> {
        Err(unsupported())
    }

    pub fn list_waitagent_session_entries(
        &self,
    ) -> Result<Vec<WaitagentSessionListEntry>, TmuxError> {
        Err(unsupported())
    }

    pub fn cleanup_stale_waitagent_socket_files(
        &self,
    ) -> Result<WaitagentSocketCleanupReport, TmuxError> {
        Err(unsupported())
    }

    pub fn set_global_option_on_socket(
        &self,
        _socket: &TmuxSocketName,
        _option_name: &str,
        _value: &str,
    ) -> Result<(), TmuxError> {
        Err(unsupported())
    }

    pub fn show_global_option_on_socket(
        &self,
        _socket: &TmuxSocketName,
        _option_name: &str,
    ) -> Result<Option<String>, TmuxError> {
        Err(unsupported())
    }

    pub fn run_socket_command(
        &self,
        _socket_name: &TmuxSocketName,
        _args: &[String],
    ) -> Result<(), TmuxError> {
        Err(unsupported())
    }

    pub fn clear_output_pipe_if_owner(
        &self,
        _workspace: &TmuxWorkspaceHandle,
        _pane: &TmuxPaneId,
    ) -> Result<(), TmuxError> {
        Err(unsupported())
    }

    pub fn kill_target(
        &self,
        _socket_name: &TmuxSocketName,
        _target_session_name: &str,
    ) -> Result<(), TmuxError> {
        Err(unsupported())
    }

    pub fn remote_runtime_owner_network(&self) -> Option<crate::cli::RemoteNetworkConfig> {
        None
    }

    pub fn runtime_signal_order(&self) -> Vec<String> {
        Vec::new()
    }

    pub fn signal_runtime_command_changed(
        &self,
        _socket_name: &str,
        _target_session_name: Option<&str>,
        _command_name: Option<&str>,
        _signal: crate::cli::RuntimeCommandSignal,
        _event_seq: Option<u64>,
    ) -> Result<(), TmuxError> {
        Err(unsupported())
    }

    pub fn wait_for_chrome_refresh_event_on_socket(
        &self,
        _socket_name: &TmuxSocketName,
        _session_name: &TmuxSessionName,
    ) -> Result<ChromeRefreshEvent, TmuxError> {
        Err(unsupported())
    }

    pub fn wait_for_chrome_refresh_on_socket(
        &self,
        _socket_name: &TmuxSocketName,
        _session_name: &TmuxSessionName,
    ) -> Result<(), TmuxError> {
        Err(unsupported())
    }

    pub fn coordinate_geometry_on_socket(
        &self,
        _socket_name: &TmuxSocketName,
        _session_name: &TmuxSessionName,
        _pane_id: &TmuxPaneId,
        _cols: u16,
        _rows: u16,
        _is_fullscreen: bool,
    ) -> Result<(), TmuxError> {
        Err(unsupported())
    }

    pub fn signal_remote_target_exited_to_workspace(
        &self,
        _socket_name: &str,
        _target_session_name: &str,
        _target: &str,
        _pane_id: Option<&str>,
    ) -> Result<usize, TmuxError> {
        Err(unsupported())
    }

    pub fn live_workspace_socket_names(
        &self,
        _network: &crate::cli::RemoteNetworkConfig,
    ) -> Result<Vec<String>, TmuxError> {
        Err(unsupported())
    }

    pub fn bind_publication(
        &self,
        _socket_name: &str,
        _target_session_name: &str,
        _authority_id: &str,
        _transport_session_id: &str,
        _selector: Option<&str>,
    ) -> Result<(), TmuxError> {
        Err(unsupported())
    }

    pub fn list_publication_bindings(
        &self,
        _socket_name: &str,
    ) -> Result<Vec<RemoteTargetPublicationBinding>, TmuxError> {
        Err(unsupported())
    }

    pub fn ensure_publication_server_running(
        &self,
        _socket_name: &str,
        _network: &crate::cli::RemoteNetworkConfig,
    ) -> Result<(), TmuxError> {
        Err(unsupported())
    }

    pub fn ensure_publication_agent_running(
        &self,
        _socket_name: &str,
        _network: &crate::cli::RemoteNetworkConfig,
    ) -> Result<(), TmuxError> {
        Err(unsupported())
    }

    pub fn ensure_publication_sender_running(
        &self,
        _socket_name: &str,
        _network: &crate::cli::RemoteNetworkConfig,
    ) -> Result<(), TmuxError> {
        Err(unsupported())
    }

    pub fn ensure_publication_owner_running(
        &self,
        _socket_name: &str,
        _target_session_name: &str,
        _network: &crate::cli::RemoteNetworkConfig,
    ) -> Result<(), TmuxError> {
        Err(unsupported())
    }

    pub fn signal_remote_node_offline(&self, _node_id: &str) -> Result<(), TmuxError> {
        Err(unsupported())
    }

    pub fn target_presentation_pane_on_socket(
        &self,
        _socket_name: &str,
        _target_session_name: &str,
    ) -> Result<TmuxPaneId, TmuxError> {
        Err(unsupported())
    }

    pub fn resize_pane_on_socket(
        &self,
        _socket_name: &str,
        _pane: &TmuxPaneId,
        _cols: usize,
        _rows: usize,
    ) -> Result<(), TmuxError> {
        Err(unsupported())
    }
}

#[derive(Debug, Clone)]
pub struct VendoredTmuxSource;

impl VendoredTmuxSource {
    pub fn new(_tmux_binary_path: PathBuf) -> Self {
        Self
    }

    pub fn discover_from_build_env() -> Result<Self, TmuxError> {
        Err(unsupported())
    }

    pub fn system_default() -> Self {
        Self
    }
}

#[derive(Debug, Clone)]
pub struct TmuxGlueBuildStatus;

impl TmuxGlueBuildStatus {
    pub fn from_build_env() -> Result<Self, TmuxError> {
        Err(unsupported())
    }
}

#[derive(Debug, Clone)]
pub struct TmuxGlueArtifacts;

impl TmuxGlueArtifacts {
    pub fn from_build_env() -> Result<Self, TmuxError> {
        Err(unsupported())
    }
}

#[derive(Debug, Clone)]
pub struct TmuxGlueBuildConfig;

impl TmuxGlueBuildConfig {
    pub fn from_artifacts(_artifacts: &TmuxGlueArtifacts) -> Self {
        Self
    }
}

impl TmuxGateway for EmbeddedTmuxBackend {
    type Error = TmuxError;

    fn ensure_workspace(
        &self,
        _config: &WorkspaceInstanceConfig,
    ) -> Result<TmuxWorkspaceHandle, Self::Error> {
        Err(unsupported())
    }

    fn create_window(
        &self,
        _workspace: &TmuxWorkspaceHandle,
        _window_name: &str,
    ) -> Result<TmuxWindowHandle, Self::Error> {
        Err(unsupported())
    }

    fn split_pane_right(
        &self,
        _workspace: &TmuxWorkspaceHandle,
        _window: &TmuxWindowHandle,
        _width_percent: u8,
    ) -> Result<TmuxPaneId, Self::Error> {
        Err(unsupported())
    }

    fn split_pane_bottom(
        &self,
        _workspace: &TmuxWorkspaceHandle,
        _window: &TmuxWindowHandle,
        _height_percent: u8,
    ) -> Result<TmuxPaneId, Self::Error> {
        Err(unsupported())
    }

    fn select_window(
        &self,
        _workspace: &TmuxWorkspaceHandle,
        _window: &TmuxWindowHandle,
    ) -> Result<(), Self::Error> {
        Err(unsupported())
    }

    fn select_pane(
        &self,
        _workspace: &TmuxWorkspaceHandle,
        _pane: &TmuxPaneId,
    ) -> Result<(), Self::Error> {
        Err(unsupported())
    }

    fn enter_copy_mode(
        &self,
        _workspace: &TmuxWorkspaceHandle,
        _pane: &TmuxPaneId,
    ) -> Result<(), Self::Error> {
        Err(unsupported())
    }
}

impl TmuxSessionGateway for EmbeddedTmuxBackend {
    fn list_sessions(&self) -> Result<Vec<ManagedSessionRecord>, Self::Error> {
        Err(unsupported())
    }

    fn list_sessions_on_socket(
        &self,
        _socket_name: &TmuxSocketName,
    ) -> Result<Vec<ManagedSessionRecord>, Self::Error> {
        Err(unsupported())
    }

    fn find_session(&self, _target: &str) -> Result<Option<ManagedSessionRecord>, Self::Error> {
        Err(unsupported())
    }

    fn attach_workspace(&self, _workspace: &TmuxWorkspaceHandle) -> Result<(), Self::Error> {
        Err(unsupported())
    }

    fn attach_session(&self, _address: &ManagedSessionAddress) -> Result<(), Self::Error> {
        Err(unsupported())
    }

    fn detach_session_clients(&self, _address: &ManagedSessionAddress) -> Result<(), Self::Error> {
        Err(unsupported())
    }

    fn detach_current_client(&self) -> Result<(), Self::Error> {
        Err(unsupported())
    }

    fn current_client_session(&self) -> Result<Option<ManagedSessionRecord>, Self::Error> {
        Err(unsupported())
    }

    fn kill_server(&self, _socket_name: &TmuxSocketName) -> Result<(), Self::Error> {
        Err(unsupported())
    }

    fn set_session_environment(
        &self,
        _socket: &TmuxSocketName,
        _session: &str,
        _key: &str,
        _value: &str,
    ) -> Result<(), Self::Error> {
        Err(unsupported())
    }

    fn unset_session_environment(
        &self,
        _socket: &TmuxSocketName,
        _session: &str,
        _key: &str,
    ) -> Result<(), Self::Error> {
        Err(unsupported())
    }

    fn show_session_environment(
        &self,
        _socket: &TmuxSocketName,
        _session: &str,
    ) -> Result<Vec<(String, String)>, Self::Error> {
        Err(unsupported())
    }
}

impl TmuxChromeGateway for EmbeddedTmuxBackend {
    fn pane_dimensions_on_socket(
        &self,
        _socket_name: &str,
        _pane_target: &str,
    ) -> Result<(usize, usize), Self::Error> {
        Err(unsupported())
    }

    fn window_zoomed_on_socket(
        &self,
        _socket_name: &str,
        _pane_target: &str,
    ) -> Result<bool, Self::Error> {
        Err(unsupported())
    }

    fn show_session_option(
        &self,
        _workspace: &TmuxWorkspaceHandle,
        _option_name: &str,
    ) -> Result<Option<String>, Self::Error> {
        Err(unsupported())
    }

    fn set_session_option(
        &self,
        _workspace: &TmuxWorkspaceHandle,
        _option_name: &str,
        _value: &str,
    ) -> Result<(), Self::Error> {
        Err(unsupported())
    }
}

impl TmuxLayoutGateway for EmbeddedTmuxBackend {
    fn current_window(
        &self,
        _workspace: &TmuxWorkspaceHandle,
    ) -> Result<TmuxWindowHandle, Self::Error> {
        Err(unsupported())
    }

    fn current_pane(&self, _workspace: &TmuxWorkspaceHandle) -> Result<TmuxPaneId, Self::Error> {
        Err(unsupported())
    }

    fn list_panes(
        &self,
        _workspace: &TmuxWorkspaceHandle,
        _window: &TmuxWindowHandle,
    ) -> Result<Vec<TmuxPaneInfo>, Self::Error> {
        Err(unsupported())
    }

    fn split_pane_right_with_program(
        &self,
        _workspace: &TmuxWorkspaceHandle,
        _pane: &TmuxPaneId,
        _width: TmuxSplitSize,
        _program: &TmuxProgram,
    ) -> Result<TmuxPaneId, Self::Error> {
        Err(unsupported())
    }

    fn split_pane_bottom_with_program(
        &self,
        _workspace: &TmuxWorkspaceHandle,
        _pane: &TmuxPaneId,
        _height: TmuxSplitSize,
        _full_width: bool,
        _program: &TmuxProgram,
    ) -> Result<TmuxPaneId, Self::Error> {
        Err(unsupported())
    }

    fn respawn_pane(
        &self,
        _workspace: &TmuxWorkspaceHandle,
        _pane: &TmuxPaneId,
        _program: &TmuxProgram,
    ) -> Result<(), Self::Error> {
        Err(unsupported())
    }

    fn set_pane_title(
        &self,
        _workspace: &TmuxWorkspaceHandle,
        _pane: &TmuxPaneId,
        _title: &str,
    ) -> Result<(), Self::Error> {
        Err(unsupported())
    }

    fn set_pane_width(
        &self,
        _workspace: &TmuxWorkspaceHandle,
        _pane: &TmuxPaneId,
        _width: u16,
    ) -> Result<(), Self::Error> {
        Err(unsupported())
    }

    fn set_pane_height(
        &self,
        _workspace: &TmuxWorkspaceHandle,
        _pane: &TmuxPaneId,
        _height: u16,
    ) -> Result<(), Self::Error> {
        Err(unsupported())
    }

    fn set_pane_style(
        &self,
        _workspace: &TmuxWorkspaceHandle,
        _pane: &TmuxPaneId,
        _style: &str,
    ) -> Result<(), Self::Error> {
        Err(unsupported())
    }

    fn set_pane_option(
        &self,
        _workspace: &TmuxWorkspaceHandle,
        _pane: &TmuxPaneId,
        _option_name: &str,
        _value: &str,
    ) -> Result<(), Self::Error> {
        Err(unsupported())
    }

    fn unset_pane_option(
        &self,
        _workspace: &TmuxWorkspaceHandle,
        _pane: &TmuxPaneId,
        _option_name: &str,
    ) -> Result<(), Self::Error> {
        Err(unsupported())
    }

    fn set_session_hook(
        &self,
        _workspace: &TmuxWorkspaceHandle,
        _hook_name: &str,
        _command: &str,
    ) -> Result<(), Self::Error> {
        Err(unsupported())
    }

    fn set_pane_hook(
        &self,
        _workspace: &TmuxWorkspaceHandle,
        _pane: &TmuxPaneId,
        _hook_name: &str,
        _command: &str,
    ) -> Result<(), Self::Error> {
        Err(unsupported())
    }

    fn unset_pane_hook(
        &self,
        _workspace: &TmuxWorkspaceHandle,
        _pane: &TmuxPaneId,
        _hook_name: &str,
    ) -> Result<(), Self::Error> {
        Err(unsupported())
    }

    fn set_global_hook(
        &self,
        _workspace: &TmuxWorkspaceHandle,
        _hook_name: &str,
        _command: &str,
    ) -> Result<(), Self::Error> {
        Err(unsupported())
    }

    fn set_session_option(
        &self,
        _workspace: &TmuxWorkspaceHandle,
        _option_name: &str,
        _value: &str,
    ) -> Result<(), Self::Error> {
        Err(unsupported())
    }

    fn set_window_option(
        &self,
        _workspace: &TmuxWorkspaceHandle,
        _window: &TmuxWindowHandle,
        _option_name: &str,
        _value: &str,
    ) -> Result<(), Self::Error> {
        Err(unsupported())
    }
}

impl TmuxControlGateway for EmbeddedTmuxBackend {
    fn bind_key_without_prefix(
        &self,
        _workspace: &TmuxWorkspaceHandle,
        _key: &str,
        _command_and_args: &[String],
    ) -> Result<(), Self::Error> {
        Err(unsupported())
    }

    fn bind_command_with_prefix(
        &self,
        _workspace: &TmuxWorkspaceHandle,
        _key: &str,
        _command: &str,
    ) -> Result<(), Self::Error> {
        Err(unsupported())
    }

    fn bind_waitagent_focus_sidebar(
        &self,
        _workspace: &TmuxWorkspaceHandle,
        _key: &str,
        _main: &TmuxPaneId,
        _sidebar: &TmuxPaneId,
        _sidebar_width: u16,
    ) -> Result<(), Self::Error> {
        Err(unsupported())
    }

    fn bind_waitagent_focus_main(
        &self,
        _workspace: &TmuxWorkspaceHandle,
        _key: &str,
        _main: &TmuxPaneId,
    ) -> Result<(), Self::Error> {
        Err(unsupported())
    }

    fn bind_waitagent_sidebar_back(
        &self,
        _workspace: &TmuxWorkspaceHandle,
        _key: &str,
        _sidebar: &TmuxPaneId,
        _main: &TmuxPaneId,
    ) -> Result<(), Self::Error> {
        Err(unsupported())
    }

    fn bind_waitagent_sidebar_hide(
        &self,
        _workspace: &TmuxWorkspaceHandle,
        _key: &str,
        _sidebar: &TmuxPaneId,
        _command: &str,
    ) -> Result<(), Self::Error> {
        Err(unsupported())
    }

    fn bind_waitagent_sidebar_toggle(
        &self,
        _workspace: &TmuxWorkspaceHandle,
        _key: &str,
        _main: &TmuxPaneId,
        _command: &str,
    ) -> Result<(), Self::Error> {
        Err(unsupported())
    }

    fn bind_waitagent_sidebar_show(
        &self,
        _workspace: &TmuxWorkspaceHandle,
        _key: &str,
        _main: &TmuxPaneId,
        _command: &str,
    ) -> Result<(), Self::Error> {
        Err(unsupported())
    }

    fn bind_waitagent_footer_action(
        &self,
        _workspace: &TmuxWorkspaceHandle,
        _key: &str,
        _footer: &TmuxPaneId,
        _command: &str,
    ) -> Result<(), Self::Error> {
        Err(unsupported())
    }

    fn bind_copy_mode_cancel_key(
        &self,
        _workspace: &TmuxWorkspaceHandle,
        _table: &str,
        _key: &str,
    ) -> Result<(), Self::Error> {
        Err(unsupported())
    }
}
