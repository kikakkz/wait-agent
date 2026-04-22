use crate::domain::workspace::{WorkspaceInstanceConfig, WorkspaceInstanceId};
use crate::infra::tmux_glue::{
    TmuxGlueArtifacts, TmuxGlueBuildConfig, TmuxGlueBuildStatus, VendoredTmuxSource,
};
use std::fmt;

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

    fn toggle_zoom(
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddedTmuxBackend {
    source: VendoredTmuxSource,
    artifacts: TmuxGlueArtifacts,
    build_status: TmuxGlueBuildStatus,
    build_config: TmuxGlueBuildConfig,
}

impl EmbeddedTmuxBackend {
    pub fn new(
        source: VendoredTmuxSource,
        artifacts: TmuxGlueArtifacts,
        build_status: TmuxGlueBuildStatus,
        build_config: TmuxGlueBuildConfig,
    ) -> Self {
        Self {
            source,
            artifacts,
            build_status,
            build_config,
        }
    }

    pub fn source(&self) -> &VendoredTmuxSource {
        &self.source
    }

    pub fn build_config(&self) -> &TmuxGlueBuildConfig {
        &self.build_config
    }

    pub fn artifacts(&self) -> &TmuxGlueArtifacts {
        &self.artifacts
    }

    pub fn build_status(&self) -> &TmuxGlueBuildStatus {
        &self.build_status
    }

    pub fn from_build_env() -> Result<Self, TmuxError> {
        let source = VendoredTmuxSource::discover_from_build_env()?;
        let artifacts = TmuxGlueArtifacts::from_build_env()?;
        let build_status = TmuxGlueBuildStatus::from_build_env()?;
        let build_config = TmuxGlueBuildConfig::from_artifacts(&artifacts);
        Ok(Self::new(source, artifacts, build_status, build_config))
    }
}

impl TmuxGateway for EmbeddedTmuxBackend {
    type Error = TmuxError;

    fn ensure_workspace(
        &self,
        config: &WorkspaceInstanceConfig,
    ) -> Result<TmuxWorkspaceHandle, Self::Error> {
        Ok(TmuxWorkspaceHandle {
            workspace_id: WorkspaceInstanceId::new(config.workspace_key.clone()),
            socket_name: TmuxSocketName::new(config.socket_name.clone()),
            session_name: TmuxSessionName::new(config.session_name.clone()),
        })
    }

    fn create_window(
        &self,
        workspace: &TmuxWorkspaceHandle,
        window_name: &str,
    ) -> Result<TmuxWindowHandle, Self::Error> {
        Ok(TmuxWindowHandle {
            workspace_id: workspace.workspace_id.clone(),
            window_id: TmuxWindowId::new(format!(
                "{}:{}",
                workspace.session_name.as_str(),
                window_name
            )),
        })
    }

    fn split_pane_right(
        &self,
        _workspace: &TmuxWorkspaceHandle,
        window: &TmuxWindowHandle,
        width_percent: u8,
    ) -> Result<TmuxPaneId, Self::Error> {
        Ok(TmuxPaneId::new(format!(
            "{}:right:{width_percent}",
            window.window_id.as_str()
        )))
    }

    fn split_pane_bottom(
        &self,
        _workspace: &TmuxWorkspaceHandle,
        window: &TmuxWindowHandle,
        height_percent: u8,
    ) -> Result<TmuxPaneId, Self::Error> {
        Ok(TmuxPaneId::new(format!(
            "{}:bottom:{height_percent}",
            window.window_id.as_str()
        )))
    }

    fn select_window(
        &self,
        _workspace: &TmuxWorkspaceHandle,
        _window: &TmuxWindowHandle,
    ) -> Result<(), Self::Error> {
        Ok(())
    }

    fn select_pane(
        &self,
        _workspace: &TmuxWorkspaceHandle,
        _pane: &TmuxPaneId,
    ) -> Result<(), Self::Error> {
        Ok(())
    }

    fn toggle_zoom(
        &self,
        _workspace: &TmuxWorkspaceHandle,
        _pane: &TmuxPaneId,
    ) -> Result<(), Self::Error> {
        Ok(())
    }

    fn enter_copy_mode(
        &self,
        _workspace: &TmuxWorkspaceHandle,
        _pane: &TmuxPaneId,
    ) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmuxError {
    message: String,
}

impl TmuxError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for TmuxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for TmuxError {}

#[cfg(test)]
mod tests {
    use super::{EmbeddedTmuxBackend, TmuxGateway, TmuxGlueBuildStatus, TmuxWorkspaceHandle};
    use crate::domain::workspace::{WorkspaceInstanceConfig, WorkspaceInstanceId};

    fn workspace_config() -> WorkspaceInstanceConfig {
        WorkspaceInstanceConfig {
            workspace_key: "wk-1".to_string(),
            socket_name: "sock-1".to_string(),
            session_name: "sess-1".to_string(),
        }
    }

    fn workspace_handle() -> TmuxWorkspaceHandle {
        TmuxWorkspaceHandle {
            workspace_id: WorkspaceInstanceId::new("wk-1"),
            socket_name: super::TmuxSocketName::new("sock-1"),
            session_name: super::TmuxSessionName::new("sess-1"),
        }
    }

    #[test]
    fn embedded_backend_returns_workspace_handle_without_shelling_out() {
        let backend = EmbeddedTmuxBackend::from_build_env()
            .expect("vendored tmux backend should discover build env");

        let handle = backend
            .ensure_workspace(&workspace_config())
            .expect("workspace handle should build");

        assert_eq!(handle.workspace_id.as_str(), "wk-1");
        assert_eq!(handle.socket_name.as_str(), "sock-1");
        assert_eq!(handle.session_name.as_str(), "sess-1");
        assert_eq!(backend.build_status(), &TmuxGlueBuildStatus::Executed);
        assert!(backend
            .artifacts()
            .static_lib_path
            .to_string_lossy()
            .ends_with("/lib/libtmux-glue.a"));
    }

    #[test]
    fn embedded_backend_uses_stable_window_and_pane_identifiers() {
        let backend = EmbeddedTmuxBackend::from_build_env()
            .expect("vendored tmux backend should discover build env");
        let workspace = workspace_handle();
        let window = super::TmuxWindowHandle {
            workspace_id: WorkspaceInstanceId::new("wk-1"),
            window_id: super::TmuxWindowId::new("@3"),
        };

        let created_window = backend
            .create_window(&workspace, "codex")
            .expect("window handle should build");
        let right = backend
            .split_pane_right(&workspace, &window, 24)
            .expect("right pane should build");
        let bottom = backend
            .split_pane_bottom(&workspace, &window, 18)
            .expect("bottom pane should build");

        assert_eq!(created_window.window_id.as_str(), "sess-1:codex");
        assert_eq!(right.as_str(), "@3:right:24");
        assert_eq!(bottom.as_str(), "@3:bottom:18");
    }
}
