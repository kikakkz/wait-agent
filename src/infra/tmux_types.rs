use crate::domain::session_catalog::{ManagedSessionAddress, ManagedSessionRecord};
use crate::domain::workspace::{WorkspaceInstanceConfig, WorkspaceInstanceId};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TmuxSocketName(String);

impl TmuxSocketName {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TmuxSessionName(String);

impl TmuxSessionName {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TmuxWindowId(String);

impl TmuxWindowId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TmuxPaneId(String);

impl TmuxPaneId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmuxWorkspaceHandle {
    pub workspace_id: WorkspaceInstanceId,
    pub socket_name: TmuxSocketName,
    pub session_name: TmuxSessionName,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmuxWindowHandle {
    pub workspace_id: WorkspaceInstanceId,
    pub window_id: TmuxWindowId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmuxPaneInfo {
    pub pane_id: TmuxPaneId,
    pub pane_pid: Option<u32>,
    pub title: String,
    pub current_command: Option<String>,
    pub current_path: Option<PathBuf>,
    pub is_dead: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TmuxSplitSize {
    Cells(u16),
    Percent(u8),
}

impl TmuxSplitSize {
    pub fn to_tmux_size(&self) -> String {
        match self {
            Self::Cells(value) => value.to_string(),
            Self::Percent(value) => format!("{value}%"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmuxProgram {
    pub program: String,
    pub args: Vec<String>,
    pub environment: Vec<(String, String)>,
    pub start_directory: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteTargetPublicationBinding {
    pub socket_name: String,
    pub target_session_name: String,
    pub authority_id: String,
    pub transport_session_id: String,
    pub selector: Option<String>,
}

impl TmuxProgram {
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            environment: Vec::new(),
            start_directory: None,
        }
    }

    pub fn with_args(mut self, args: impl IntoIterator<Item = String>) -> Self {
        self.args = args.into_iter().collect();
        self
    }

    pub fn with_environment(
        mut self,
        environment: impl IntoIterator<Item = (String, String)>,
    ) -> Self {
        self.environment = environment.into_iter().collect();
        self
    }

    pub fn with_start_directory(mut self, directory: impl Into<PathBuf>) -> Self {
        self.start_directory = Some(directory.into());
        self
    }

    pub fn binary_name(&self) -> &str {
        self.program
            .rsplit('/')
            .next()
            .unwrap_or(self.program.as_str())
    }
}

pub trait TmuxGateway {
    type Error;

    fn ensure_workspace(
        &self,
        config: &WorkspaceInstanceConfig,
    ) -> Result<TmuxWorkspaceHandle, Self::Error>;

    fn create_window(
        &self,
        workspace: &TmuxWorkspaceHandle,
        window_name: &str,
    ) -> Result<TmuxWindowHandle, Self::Error>;

    fn split_pane_right(
        &self,
        workspace: &TmuxWorkspaceHandle,
        window: &TmuxWindowHandle,
        width_percent: u8,
    ) -> Result<TmuxPaneId, Self::Error>;

    fn split_pane_bottom(
        &self,
        workspace: &TmuxWorkspaceHandle,
        window: &TmuxWindowHandle,
        height_percent: u8,
    ) -> Result<TmuxPaneId, Self::Error>;

    fn select_window(
        &self,
        workspace: &TmuxWorkspaceHandle,
        window: &TmuxWindowHandle,
    ) -> Result<(), Self::Error>;

    fn select_pane(
        &self,
        workspace: &TmuxWorkspaceHandle,
        pane: &TmuxPaneId,
    ) -> Result<(), Self::Error>;

    fn enter_copy_mode(
        &self,
        workspace: &TmuxWorkspaceHandle,
        pane: &TmuxPaneId,
    ) -> Result<(), Self::Error>;
}

pub trait TmuxSessionGateway: TmuxGateway {
    fn list_sessions(&self) -> Result<Vec<ManagedSessionRecord>, Self::Error>;

    fn list_sessions_on_socket(
        &self,
        socket_name: &TmuxSocketName,
    ) -> Result<Vec<ManagedSessionRecord>, Self::Error>;

    fn find_session(&self, target: &str) -> Result<Option<ManagedSessionRecord>, Self::Error>;

    fn attach_workspace(&self, workspace: &TmuxWorkspaceHandle) -> Result<(), Self::Error>;

    fn attach_session(&self, address: &ManagedSessionAddress) -> Result<(), Self::Error>;

    fn detach_session_clients(&self, address: &ManagedSessionAddress) -> Result<(), Self::Error>;

    fn detach_current_client(&self) -> Result<(), Self::Error>;

    fn current_client_session(&self) -> Result<Option<ManagedSessionRecord>, Self::Error>;
}

pub trait TmuxChromeGateway: TmuxSessionGateway {
    fn pane_dimensions_on_socket(
        &self,
        socket_name: &str,
        pane_target: &str,
    ) -> Result<(usize, usize), Self::Error>;

    fn window_zoomed_on_socket(
        &self,
        socket_name: &str,
        pane_target: &str,
    ) -> Result<bool, Self::Error>;

    fn show_session_option(
        &self,
        workspace: &TmuxWorkspaceHandle,
        option_name: &str,
    ) -> Result<Option<String>, Self::Error>;
}

pub trait TmuxLayoutGateway: TmuxGateway {
    fn current_window(
        &self,
        workspace: &TmuxWorkspaceHandle,
    ) -> Result<TmuxWindowHandle, Self::Error>;

    fn current_pane(&self, workspace: &TmuxWorkspaceHandle) -> Result<TmuxPaneId, Self::Error>;

    fn list_panes(
        &self,
        workspace: &TmuxWorkspaceHandle,
        window: &TmuxWindowHandle,
    ) -> Result<Vec<TmuxPaneInfo>, Self::Error>;

    fn split_pane_right_with_program(
        &self,
        workspace: &TmuxWorkspaceHandle,
        pane: &TmuxPaneId,
        width: TmuxSplitSize,
        program: &TmuxProgram,
    ) -> Result<TmuxPaneId, Self::Error>;

    fn split_pane_bottom_with_program(
        &self,
        workspace: &TmuxWorkspaceHandle,
        pane: &TmuxPaneId,
        height: TmuxSplitSize,
        full_width: bool,
        program: &TmuxProgram,
    ) -> Result<TmuxPaneId, Self::Error>;

    fn respawn_pane(
        &self,
        workspace: &TmuxWorkspaceHandle,
        pane: &TmuxPaneId,
        program: &TmuxProgram,
    ) -> Result<(), Self::Error>;

    fn set_pane_title(
        &self,
        workspace: &TmuxWorkspaceHandle,
        pane: &TmuxPaneId,
        title: &str,
    ) -> Result<(), Self::Error>;

    fn set_pane_width(
        &self,
        workspace: &TmuxWorkspaceHandle,
        pane: &TmuxPaneId,
        width: u16,
    ) -> Result<(), Self::Error>;

    fn set_pane_height(
        &self,
        workspace: &TmuxWorkspaceHandle,
        pane: &TmuxPaneId,
        height: u16,
    ) -> Result<(), Self::Error>;

    fn set_pane_style(
        &self,
        workspace: &TmuxWorkspaceHandle,
        pane: &TmuxPaneId,
        style: &str,
    ) -> Result<(), Self::Error>;

    fn set_pane_option(
        &self,
        workspace: &TmuxWorkspaceHandle,
        pane: &TmuxPaneId,
        option_name: &str,
        value: &str,
    ) -> Result<(), Self::Error>;

    fn set_session_hook(
        &self,
        workspace: &TmuxWorkspaceHandle,
        hook_name: &str,
        command: &str,
    ) -> Result<(), Self::Error>;

    fn set_pane_hook(
        &self,
        workspace: &TmuxWorkspaceHandle,
        pane: &TmuxPaneId,
        hook_name: &str,
        command: &str,
    ) -> Result<(), Self::Error>;

    fn set_global_hook(
        &self,
        workspace: &TmuxWorkspaceHandle,
        hook_name: &str,
        command: &str,
    ) -> Result<(), Self::Error>;

    fn set_session_option(
        &self,
        workspace: &TmuxWorkspaceHandle,
        option_name: &str,
        value: &str,
    ) -> Result<(), Self::Error>;

    fn set_window_option(
        &self,
        workspace: &TmuxWorkspaceHandle,
        window: &TmuxWindowHandle,
        option_name: &str,
        value: &str,
    ) -> Result<(), Self::Error>;
}

pub trait TmuxControlGateway: TmuxLayoutGateway {
    fn bind_key_without_prefix(
        &self,
        workspace: &TmuxWorkspaceHandle,
        key: &str,
        command_and_args: &[String],
    ) -> Result<(), Self::Error>;

    fn bind_command_with_prefix(
        &self,
        workspace: &TmuxWorkspaceHandle,
        key: &str,
        command: &str,
    ) -> Result<(), Self::Error>;

    fn bind_waitagent_focus_sidebar(
        &self,
        workspace: &TmuxWorkspaceHandle,
        key: &str,
        main: &TmuxPaneId,
        sidebar: &TmuxPaneId,
        sidebar_width: u16,
    ) -> Result<(), Self::Error>;

    fn bind_waitagent_focus_main(
        &self,
        workspace: &TmuxWorkspaceHandle,
        key: &str,
        main: &TmuxPaneId,
    ) -> Result<(), Self::Error>;

    fn bind_waitagent_sidebar_back(
        &self,
        workspace: &TmuxWorkspaceHandle,
        key: &str,
        sidebar: &TmuxPaneId,
        main: &TmuxPaneId,
    ) -> Result<(), Self::Error>;

    fn bind_waitagent_sidebar_hide(
        &self,
        workspace: &TmuxWorkspaceHandle,
        key: &str,
        sidebar: &TmuxPaneId,
        main: &TmuxPaneId,
        collapsed_width: u16,
    ) -> Result<(), Self::Error>;

    fn bind_waitagent_footer_action(
        &self,
        workspace: &TmuxWorkspaceHandle,
        key: &str,
        footer: &TmuxPaneId,
        command: &str,
    ) -> Result<(), Self::Error>;
}
