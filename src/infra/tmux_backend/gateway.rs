use super::{EmbeddedTmuxBackend, TmuxError};
use crate::domain::session_catalog::{ManagedSessionAddress, ManagedSessionRecord};
use crate::domain::workspace::WorkspaceInstanceConfig;
use crate::infra::tmux_error::{parse_tmux_id, validate_percent};
use crate::infra::tmux_types::{
    TmuxChromeGateway, TmuxGateway, TmuxPaneId, TmuxSessionGateway, TmuxSocketName,
    TmuxWindowHandle, TmuxWindowId, TmuxWorkspaceHandle,
};

// Channel helpers — free functions used by the main EmbeddedTmuxBackend impl

pub(super) fn workspace_chrome_refresh_channel(session_name: &str) -> String {
    format!(
        "{}-{session_name}",
        crate::infra::tmux_backend::WAITAGENT_CHROME_REFRESH_CHANNEL_PREFIX
    )
}

pub(super) fn workspace_sidebar_ready_channel(session_name: &str) -> String {
    format!(
        "{}-{session_name}",
        crate::infra::tmux_backend::WAITAGENT_SIDEBAR_READY_CHANNEL_PREFIX
    )
}

pub(super) fn workspace_footer_ready_channel(session_name: &str) -> String {
    format!(
        "{}-{session_name}",
        crate::infra::tmux_backend::WAITAGENT_FOOTER_READY_CHANNEL_PREFIX
    )
}

pub(super) fn default_window_name() -> String {
    default_shell_path()
        .and_then(|value| {
            std::path::Path::new(&value)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "shell".to_string())
}

pub(super) fn default_shell_path() -> Option<String> {
    std::env::var("SHELL")
        .ok()
        .filter(|value| !value.is_empty())
}

impl TmuxGateway for EmbeddedTmuxBackend {
    type Error = TmuxError;

    fn ensure_workspace(
        &self,
        config: &WorkspaceInstanceConfig,
    ) -> Result<TmuxWorkspaceHandle, Self::Error> {
        let workspace = Self::workspace_handle_for_config(config);
        if !self.session_exists(&workspace)? {
            self.create_workspace_session(config, &workspace)?;
        }
        self.sync_workspace_metadata(config, &workspace)?;
        Ok(workspace)
    }

    fn create_window(
        &self,
        workspace: &TmuxWorkspaceHandle,
        window_name: &str,
    ) -> Result<TmuxWindowHandle, Self::Error> {
        let args = vec![
            "new-window".to_string(),
            "-d".to_string(),
            "-P".to_string(),
            "-F".to_string(),
            "#{window_id}".to_string(),
            "-t".to_string(),
            workspace.session_name.as_str().to_string(),
            "-n".to_string(),
            window_name.to_string(),
        ];
        let output = self.run_workspace_command(workspace, &args)?;
        let window_id = parse_tmux_id(&output.stdout, '@', "window id")?;
        Ok(TmuxWindowHandle {
            workspace_id: workspace.workspace_id.clone(),
            window_id: TmuxWindowId::new(window_id),
        })
    }

    fn split_pane_right(
        &self,
        workspace: &TmuxWorkspaceHandle,
        window: &TmuxWindowHandle,
        width_percent: u8,
    ) -> Result<TmuxPaneId, Self::Error> {
        validate_percent(width_percent, "right split width")?;
        let args = vec![
            "split-window".to_string(),
            "-d".to_string(),
            "-P".to_string(),
            "-F".to_string(),
            "#{pane_id}".to_string(),
            "-t".to_string(),
            window.window_id.as_str().to_string(),
            "-h".to_string(),
            "-l".to_string(),
            format!("{width_percent}%"),
        ];
        let output = self.run_workspace_command(workspace, &args)?;
        Ok(TmuxPaneId::new(parse_tmux_id(
            &output.stdout,
            '%',
            "pane id",
        )?))
    }

    fn split_pane_bottom(
        &self,
        workspace: &TmuxWorkspaceHandle,
        window: &TmuxWindowHandle,
        height_percent: u8,
    ) -> Result<TmuxPaneId, Self::Error> {
        validate_percent(height_percent, "bottom split height")?;
        let args = vec![
            "split-window".to_string(),
            "-d".to_string(),
            "-P".to_string(),
            "-F".to_string(),
            "#{pane_id}".to_string(),
            "-t".to_string(),
            window.window_id.as_str().to_string(),
            "-v".to_string(),
            "-l".to_string(),
            format!("{height_percent}%"),
        ];
        let output = self.run_workspace_command(workspace, &args)?;
        Ok(TmuxPaneId::new(parse_tmux_id(
            &output.stdout,
            '%',
            "pane id",
        )?))
    }

    fn select_window(
        &self,
        workspace: &TmuxWorkspaceHandle,
        window: &TmuxWindowHandle,
    ) -> Result<(), Self::Error> {
        let args = vec![
            "select-window".to_string(),
            "-t".to_string(),
            window.window_id.as_str().to_string(),
        ];
        self.run_workspace_command(workspace, &args)?;
        Ok(())
    }

    fn select_pane(
        &self,
        workspace: &TmuxWorkspaceHandle,
        pane: &TmuxPaneId,
    ) -> Result<(), Self::Error> {
        let args = vec![
            "select-pane".to_string(),
            "-t".to_string(),
            pane.as_str().to_string(),
        ];
        self.run_workspace_command(workspace, &args)?;
        Ok(())
    }

    fn enter_copy_mode(
        &self,
        workspace: &TmuxWorkspaceHandle,
        pane: &TmuxPaneId,
    ) -> Result<(), Self::Error> {
        let args = vec![
            "copy-mode".to_string(),
            "-t".to_string(),
            pane.as_str().to_string(),
        ];
        self.run_workspace_command(workspace, &args)?;
        Ok(())
    }
}

impl TmuxSessionGateway for EmbeddedTmuxBackend {
    fn list_sessions(&self) -> Result<Vec<ManagedSessionRecord>, Self::Error> {
        let mut sessions = Vec::new();
        for socket_name in self.discover_waitagent_sockets()? {
            sessions.extend(self.list_sessions_on_socket(&socket_name)?);
        }
        sessions.sort_by(|left, right| {
            left.address
                .server_id()
                .cmp(right.address.server_id())
                .then_with(|| left.address.session_id().cmp(right.address.session_id()))
        });
        Ok(sessions)
    }

    fn list_sessions_on_socket(
        &self,
        socket_name: &TmuxSocketName,
    ) -> Result<Vec<ManagedSessionRecord>, Self::Error> {
        EmbeddedTmuxBackend::list_sessions_on_socket(self, socket_name)
    }

    fn find_session(&self, target: &str) -> Result<Option<ManagedSessionRecord>, Self::Error> {
        self.find_managed_session(target)
    }

    fn attach_workspace(&self, workspace: &TmuxWorkspaceHandle) -> Result<(), Self::Error> {
        self.attach_to_socket_session(&workspace.socket_name, workspace.session_name.as_str())
    }

    fn attach_session(&self, address: &ManagedSessionAddress) -> Result<(), Self::Error> {
        self.attach_to_socket_session(
            &TmuxSocketName::new(address.server_id()),
            address.session_id(),
        )
    }

    fn detach_session_clients(&self, address: &ManagedSessionAddress) -> Result<(), Self::Error> {
        self.detach_session_on_socket(
            &TmuxSocketName::new(address.server_id()),
            address.session_id(),
        )
    }

    fn detach_current_client(&self) -> Result<(), Self::Error> {
        self.command_runner()
            .run_from_current_client(&["detach-client".to_string()])
    }

    fn kill_server(&self, socket_name: &TmuxSocketName) -> Result<(), Self::Error> {
        self.run_socket_command(socket_name, &["kill-server".to_string()])
    }

    fn current_client_session(&self) -> Result<Option<ManagedSessionRecord>, Self::Error> {
        let socket_name = current_client_socket_name()?;
        let output = self.command_runner().capture_from_current_client(&[
            "display-message".to_string(),
            "-p".to_string(),
            "#{session_name}".to_string(),
        ])?;
        let session_name = output.stdout.trim();
        if session_name.is_empty() {
            return Ok(None);
        }
        Ok(self
            .list_sessions_on_socket(&socket_name)?
            .into_iter()
            .find(|session| session.address.session_id() == session_name))
    }

    fn set_session_environment(
        &self,
        socket: &TmuxSocketName,
        session: &str,
        key: &str,
        value: &str,
    ) -> Result<(), Self::Error> {
        self.run_on_socket(
            socket,
            &[
                "set-environment".to_string(),
                "-t".to_string(),
                session.to_string(),
                key.to_string(),
                value.to_string(),
            ],
        )
        .map(|_| ())
    }

    fn unset_session_environment(
        &self,
        socket: &TmuxSocketName,
        session: &str,
        key: &str,
    ) -> Result<(), Self::Error> {
        self.run_on_socket(
            socket,
            &[
                "set-environment".to_string(),
                "-u".to_string(),
                "-t".to_string(),
                session.to_string(),
                key.to_string(),
            ],
        )
        .map(|_| ())
    }

    fn show_session_environment(
        &self,
        socket: &TmuxSocketName,
        session: &str,
    ) -> Result<Vec<(String, String)>, Self::Error> {
        let output = self.run_on_socket(
            socket,
            &[
                "show-environment".to_string(),
                "-t".to_string(),
                session.to_string(),
            ],
        )?;
        let mut vars = Vec::new();
        for line in output.stdout.lines() {
            if let Some((key, value)) = line.split_once('=') {
                vars.push((key.to_string(), value.to_string()));
            }
        }
        Ok(vars)
    }
}

fn current_client_socket_name() -> Result<TmuxSocketName, TmuxError> {
    let tmux = std::env::var("TMUX")
        .map_err(|_| TmuxError::new("TMUX is not set for current client session lookup"))?;
    let socket_path = tmux
        .split(',')
        .next()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| TmuxError::new("TMUX does not contain a socket path"))?;
    let socket_name = std::path::Path::new(socket_path)
        .file_name()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            TmuxError::new(format!(
                "TMUX socket path `{socket_path}` does not have a valid socket name"
            ))
        })?;
    Ok(TmuxSocketName::new(socket_name))
}

impl TmuxChromeGateway for EmbeddedTmuxBackend {
    fn pane_dimensions_on_socket(
        &self,
        socket_name: &str,
        pane_target: &str,
    ) -> Result<(usize, usize), Self::Error> {
        EmbeddedTmuxBackend::pane_dimensions_on_socket(self, socket_name, pane_target)
    }

    fn window_zoomed_on_socket(
        &self,
        socket_name: &str,
        pane_target: &str,
    ) -> Result<bool, Self::Error> {
        EmbeddedTmuxBackend::window_zoomed_on_socket(self, socket_name, pane_target)
    }

    fn show_session_option(
        &self,
        workspace: &TmuxWorkspaceHandle,
        option_name: &str,
    ) -> Result<Option<String>, Self::Error> {
        EmbeddedTmuxBackend::show_session_option(self, workspace, option_name)
    }
}
