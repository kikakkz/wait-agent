use crate::application::control_service::{ControlService, FooterMenuBindings};
use crate::application::layout_service::{
    LayoutFocusBehavior, LayoutService, FOOTER_PANE_TITLE, SIDEBAR_PANE_TITLE,
};
use crate::application::session_service::SessionService;
use crate::cli::{CloseSessionCommand, LayoutReconcileCommand};
use crate::domain::workspace::WorkspaceInstanceId;
use crate::infra::tmux::{
    EmbeddedTmuxBackend, TmuxError, TmuxLayoutGateway, TmuxProgram, TmuxSessionName,
    TmuxSocketName, TmuxWorkspaceHandle,
};
use crate::lifecycle::LifecycleError;
use std::io;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

const STARTUP_CHROME_READY_TIMEOUT: Duration = Duration::from_millis(300);
const STARTUP_CHROME_READY_POLL_INTERVAL: Duration = Duration::from_millis(10);
const WAITAGENT_MAIN_PANE_OPTION: &str = "@waitagent_main_pane_id";

pub struct WorkspaceLayoutRuntime {
    backend: EmbeddedTmuxBackend,
    control_service: ControlService<EmbeddedTmuxBackend>,
    layout_service: LayoutService<EmbeddedTmuxBackend>,
    session_service: SessionService<EmbeddedTmuxBackend>,
    current_executable: std::path::PathBuf,
}

impl WorkspaceLayoutRuntime {
    pub fn from_build_env() -> Result<Self, LifecycleError> {
        let backend = EmbeddedTmuxBackend::from_build_env().map_err(tmux_layout_error)?;
        let current_executable = std::env::current_exe().map_err(|error| {
            LifecycleError::Io(
                "failed to locate current waitagent executable".to_string(),
                error,
            )
        })?;

        Ok(Self {
            control_service: ControlService::new(backend.clone()),
            layout_service: LayoutService::new(backend.clone()),
            session_service: SessionService::new(backend.clone()),
            backend,
            current_executable,
        })
    }

    pub fn ensure_layout(
        &self,
        workspace: &TmuxWorkspaceHandle,
        workspace_dir: &Path,
    ) -> Result<(), LifecycleError> {
        let layout = self.ensure_layout_topology(
            workspace,
            workspace_dir,
            LayoutFocusBehavior::ReturnToMain,
        )?;
        self.wait_for_initial_chrome_render(workspace, &layout);
        Ok(())
    }

    pub fn sync_main_slot_bindings(
        &self,
        workspace: &TmuxWorkspaceHandle,
        workspace_dir: &Path,
    ) -> Result<(), LifecycleError> {
        self.ensure_layout_topology(workspace, workspace_dir, LayoutFocusBehavior::ReturnToMain)
            .map(|_| ())
    }

    pub fn refresh_workspace_chrome(
        &self,
        workspace: &TmuxWorkspaceHandle,
        workspace_dir: &Path,
    ) -> Result<(), LifecycleError> {
        if self.notify_existing_chrome_panes(workspace)? {
            Ok(())
        } else {
            self.refresh_chrome(workspace, workspace_dir)
        }
    }

    pub fn run_reconcile(&self, command: LayoutReconcileCommand) -> Result<(), LifecycleError> {
        let workspace_dir = PathBuf::from(&command.workspace_dir);
        let workspace = TmuxWorkspaceHandle {
            workspace_id: WorkspaceInstanceId::new(command.session_name.clone()),
            socket_name: TmuxSocketName::new(command.socket_name),
            session_name: TmuxSessionName::new(command.session_name),
        };
        self.reconcile_layout(
            &workspace,
            &workspace_dir,
            LayoutFocusBehavior::PreserveCurrent,
        )
    }

    pub fn run_chrome_refresh(
        &self,
        command: LayoutReconcileCommand,
    ) -> Result<(), LifecycleError> {
        let workspace_dir = PathBuf::from(&command.workspace_dir);
        let workspace = TmuxWorkspaceHandle {
            workspace_id: WorkspaceInstanceId::new(command.session_name.clone()),
            socket_name: TmuxSocketName::new(command.socket_name),
            session_name: TmuxSessionName::new(command.session_name),
        };
        self.refresh_chrome(&workspace, &workspace_dir)
    }

    pub fn run_chrome_refresh_all(&self) -> Result<(), LifecycleError> {
        let sessions = self
            .session_service
            .list_sessions()
            .map_err(tmux_layout_error)?;

        for session in sessions.into_iter().filter(should_refresh_workspace_chrome) {
            let Some(workspace_dir) = session.workspace_dir.as_ref() else {
                continue;
            };
            let workspace = TmuxWorkspaceHandle {
                workspace_id: WorkspaceInstanceId::new(session.address.session_id()),
                socket_name: TmuxSocketName::new(session.address.server_id()),
                session_name: TmuxSessionName::new(session.address.session_id()),
            };
            self.refresh_chrome(&workspace, workspace_dir)?;
        }

        Ok(())
    }

    pub fn run_close_session(&self, command: CloseSessionCommand) -> Result<(), LifecycleError> {
        self.backend
            .run_socket_command(
                &TmuxSocketName::new(command.socket_name),
                &[
                    "kill-session".to_string(),
                    "-t".to_string(),
                    command.session_name,
                ],
            )
            .map_err(tmux_layout_error)?;
        self.run_chrome_refresh_all()
    }

    fn reconcile_layout(
        &self,
        workspace: &TmuxWorkspaceHandle,
        workspace_dir: &Path,
        focus_behavior: LayoutFocusBehavior,
    ) -> Result<(), LifecycleError> {
        self.ensure_layout_topology(workspace, workspace_dir, focus_behavior)
            .map(|_| ())
    }

    fn refresh_chrome(
        &self,
        workspace: &TmuxWorkspaceHandle,
        workspace_dir: &Path,
    ) -> Result<(), LifecycleError> {
        self.ensure_layout_topology(
            workspace,
            workspace_dir,
            LayoutFocusBehavior::PreserveCurrent,
        )?;
        self.backend
            .signal_chrome_refresh_on_socket(
                workspace.socket_name.as_str(),
                workspace.session_name.as_str(),
            )
            .map_err(tmux_layout_error)
    }

    fn notify_existing_chrome_panes(
        &self,
        workspace: &TmuxWorkspaceHandle,
    ) -> Result<bool, LifecycleError> {
        let window = self
            .backend
            .current_window(workspace)
            .map_err(tmux_layout_error)?;
        let panes = self
            .backend
            .list_panes(workspace, &window)
            .map_err(tmux_layout_error)?;
        let Some(sidebar) = panes
            .iter()
            .find(|pane| pane.title == SIDEBAR_PANE_TITLE && !pane.is_dead)
        else {
            return Ok(false);
        };
        let Some(footer) = panes
            .iter()
            .find(|pane| pane.title == FOOTER_PANE_TITLE && !pane.is_dead)
        else {
            return Ok(false);
        };

        let _ = sidebar;
        let _ = footer;
        self.backend
            .signal_chrome_refresh_on_socket(
                workspace.socket_name.as_str(),
                workspace.session_name.as_str(),
            )
            .map_err(tmux_layout_error)?;
        Ok(true)
    }

    fn ensure_layout_topology(
        &self,
        workspace: &TmuxWorkspaceHandle,
        workspace_dir: &Path,
        focus_behavior: LayoutFocusBehavior,
    ) -> Result<crate::domain::workspace_layout::WorkspaceChromeLayout, LifecycleError> {
        let sidebar_program = self.sidebar_program(workspace, workspace_dir);
        let footer_program = self.footer_program(workspace, workspace_dir);
        let reconcile_command = self.layout_reconcile_hook_command(workspace, workspace_dir);
        let global_reconcile_command = self.chrome_refresh_all_hook_command();
        let pane_died_command = self.main_pane_died_hook_command(workspace);
        let layout = self
            .layout_service
            .ensure_workspace_layout(workspace, &sidebar_program, &footer_program, focus_behavior)
            .map_err(tmux_layout_error)?;
        let footer_bindings = self.footer_menu_bindings(workspace);
        self.control_service
            .ensure_native_controls(workspace, &layout, Some(&footer_bindings))
            .map_err(tmux_layout_error)?;
        self.backend
            .set_session_option(
                workspace,
                WAITAGENT_MAIN_PANE_OPTION,
                layout.main_pane.as_str(),
            )
            .map_err(tmux_layout_error)?;
        self.layout_service
            .ensure_layout_hooks(
                workspace,
                &layout.main_pane,
                &reconcile_command,
                &global_reconcile_command,
                &pane_died_command,
            )
            .map_err(tmux_layout_error)?;
        Ok(layout)
    }

    fn wait_for_initial_chrome_render(
        &self,
        workspace: &TmuxWorkspaceHandle,
        layout: &crate::domain::workspace_layout::WorkspaceChromeLayout,
    ) {
        let deadline = Instant::now() + STARTUP_CHROME_READY_TIMEOUT;
        loop {
            let sidebar_ready = self
                .backend
                .capture_pane_text_on_socket(
                    workspace.socket_name.as_str(),
                    layout.sidebar_pane.as_str(),
                )
                .map(|text| text.contains("Sessions"))
                .unwrap_or(false);
            let footer_ready = self
                .backend
                .capture_pane_text_on_socket(
                    workspace.socket_name.as_str(),
                    layout.footer_pane.as_str(),
                )
                .map(|text| text.contains("keys: ^W cmd"))
                .unwrap_or(false);

            if sidebar_ready && footer_ready {
                return;
            }

            if Instant::now() >= deadline {
                return;
            }

            thread::sleep(STARTUP_CHROME_READY_POLL_INTERVAL);
        }
    }

    fn sidebar_program(
        &self,
        workspace: &TmuxWorkspaceHandle,
        workspace_dir: &Path,
    ) -> TmuxProgram {
        TmuxProgram::new(self.current_executable.display().to_string())
            .with_args(vec![
                "__ui-sidebar".to_string(),
                "--socket-name".to_string(),
                workspace.socket_name.as_str().to_string(),
                "--session-name".to_string(),
                workspace.session_name.as_str().to_string(),
            ])
            .with_start_directory(workspace_dir)
    }

    fn footer_program(&self, workspace: &TmuxWorkspaceHandle, workspace_dir: &Path) -> TmuxProgram {
        TmuxProgram::new(self.current_executable.display().to_string())
            .with_args(vec![
                "__ui-footer".to_string(),
                "--socket-name".to_string(),
                workspace.socket_name.as_str().to_string(),
                "--session-name".to_string(),
                workspace.session_name.as_str().to_string(),
            ])
            .with_start_directory(workspace_dir)
    }

    fn footer_menu_bindings(&self, workspace: &TmuxWorkspaceHandle) -> FooterMenuBindings {
        let create_target_shell_command = new_target_shell_command(
            self.current_executable.to_string_lossy().as_ref(),
            workspace,
        );
        let shell_command = footer_menu_shell_command(
            self.current_executable.to_string_lossy().as_ref(),
            workspace,
        );
        FooterMenuBindings {
            create_session_command: format!(
                "run-shell -b {}",
                tmux_quote_argument(&create_target_shell_command)
            ),
            open_sessions_menu_command: format!(
                "run-shell -b {}",
                tmux_quote_argument(&shell_command)
            ),
        }
    }

    fn layout_reconcile_hook_command(
        &self,
        workspace: &TmuxWorkspaceHandle,
        workspace_dir: &Path,
    ) -> String {
        let shell_command = layout_reconcile_hook_shell_command(
            self.current_executable.to_string_lossy().as_ref(),
            workspace,
            &workspace_dir.display().to_string(),
        );
        format!(
            "run-shell -b {}",
            tmux_quote_argument(&format!("{shell_command} >/dev/null 2>&1"))
        )
    }

    fn chrome_refresh_all_hook_command(&self) -> String {
        let shell_command = chrome_refresh_all_hook_shell_command(
            self.current_executable.to_string_lossy().as_ref(),
        );
        format!(
            "run-shell -b {}",
            tmux_quote_argument(&format!("{shell_command} >/dev/null 2>&1"))
        )
    }

    fn main_pane_died_hook_command(&self, workspace: &TmuxWorkspaceHandle) -> String {
        let shell_command = main_pane_died_hook_shell_command(
            self.current_executable.to_string_lossy().as_ref(),
            workspace.socket_name.as_str(),
            workspace.session_name.as_str(),
        );
        format!(
            "run-shell -b {}",
            tmux_quote_argument(&format!("{shell_command} >/dev/null 2>&1"))
        )
    }
}

fn footer_menu_shell_command(executable: &str, workspace: &TmuxWorkspaceHandle) -> String {
    [
        shell_escape(executable),
        shell_escape("__footer-menu"),
        shell_escape("--socket-name"),
        shell_escape(workspace.socket_name.as_str()),
        shell_escape("--session-name"),
        shell_escape(workspace.session_name.as_str()),
        shell_escape("--client-tty"),
        shell_escape("#{client_tty}"),
    ]
    .join(" ")
}

fn new_target_shell_command(executable: &str, workspace: &TmuxWorkspaceHandle) -> String {
    [
        shell_escape(executable),
        shell_escape("__new-target"),
        shell_escape("--current-socket-name"),
        shell_escape(workspace.socket_name.as_str()),
        shell_escape("--current-session-name"),
        shell_escape(workspace.session_name.as_str()),
    ]
    .join(" ")
}

fn layout_reconcile_hook_shell_command(
    executable: &str,
    workspace: &TmuxWorkspaceHandle,
    workspace_dir: &str,
) -> String {
    [
        shell_escape(executable),
        shell_escape("__layout-reconcile"),
        shell_escape("--socket-name"),
        shell_escape(workspace.socket_name.as_str()),
        shell_escape("--session-name"),
        shell_escape(workspace.session_name.as_str()),
        shell_escape("--workspace-dir"),
        shell_escape(workspace_dir),
    ]
    .join(" ")
}

fn chrome_refresh_all_hook_shell_command(executable: &str) -> String {
    [
        shell_escape(executable),
        shell_escape("__chrome-refresh-all"),
    ]
    .join(" ")
}

fn main_pane_died_hook_shell_command(
    executable: &str,
    socket_name: &str,
    session_name: &str,
) -> String {
    [
        shell_escape(executable),
        shell_escape("__main-pane-died"),
        shell_escape("--socket-name"),
        shell_escape(socket_name),
        shell_escape("--session-name"),
        shell_escape(session_name),
        shell_escape("--pane-id"),
        shell_escape("#{hook_pane}"),
    ]
    .join(" ")
}

fn shell_escape(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn tmux_quote_argument(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn tmux_layout_error(error: TmuxError) -> LifecycleError {
    LifecycleError::Io(
        "failed to ensure tmux-owned waitagent layout".to_string(),
        io::Error::new(io::ErrorKind::Other, error.to_string()),
    )
}

fn should_refresh_workspace_chrome(
    session: &crate::domain::session_catalog::ManagedSessionRecord,
) -> bool {
    !matches!(
        session.session_role,
        Some(crate::domain::workspace::WorkspaceSessionRole::TargetHost)
    )
}

#[cfg(test)]
mod tests {
    use super::{
        chrome_refresh_all_hook_shell_command, footer_menu_shell_command,
        layout_reconcile_hook_shell_command, main_pane_died_hook_shell_command,
        should_refresh_workspace_chrome, tmux_quote_argument,
    };
    use crate::domain::session_catalog::{
        ManagedSessionAddress, ManagedSessionRecord, ManagedSessionTaskState,
    };
    use crate::domain::workspace::WorkspaceInstanceId;
    use crate::domain::workspace::WorkspaceSessionRole;
    use crate::infra::tmux::{TmuxSessionName, TmuxSocketName, TmuxWorkspaceHandle};
    use std::path::PathBuf;

    #[test]
    fn footer_menu_shell_command_quotes_shell_arguments_but_not_tmux_layer() {
        let workspace = workspace();

        let command = footer_menu_shell_command("/tmp/wait agent", &workspace);

        assert_eq!(
            command,
            "'/tmp/wait agent' '__footer-menu' '--socket-name' 'wa-1' '--session-name' 'session-1' '--client-tty' '#{client_tty}'"
        );
    }

    #[test]
    fn tmux_quote_argument_wraps_shell_command_for_tmux_parser() {
        let quoted =
            tmux_quote_argument("'waitagent' '__footer-menu' '--client-tty' '#{client_tty}'");

        assert_eq!(
            quoted,
            "\"'waitagent' '__footer-menu' '--client-tty' '#{client_tty}'\""
        );
    }

    #[test]
    fn layout_reconcile_hook_shell_command_preserves_workspace_directory_as_shell_argument() {
        let workspace = workspace();

        let command =
            layout_reconcile_hook_shell_command("/tmp/wait agent", &workspace, "/tmp/demo path");

        assert_eq!(
            command,
            "'/tmp/wait agent' '__layout-reconcile' '--socket-name' 'wa-1' '--session-name' 'session-1' '--workspace-dir' '/tmp/demo path'"
        );
    }

    #[test]
    fn chrome_refresh_all_hook_shell_command_runs_global_refresh() {
        let command = chrome_refresh_all_hook_shell_command("/tmp/wait agent");

        assert_eq!(command, "'/tmp/wait agent' '__chrome-refresh-all'");
    }

    #[test]
    fn main_pane_died_hook_shell_command_targets_current_session() {
        let command = main_pane_died_hook_shell_command("/tmp/wait agent", "wa-1", "session-1");

        assert_eq!(
            command,
            "'/tmp/wait agent' '__main-pane-died' '--socket-name' 'wa-1' '--session-name' 'session-1' '--pane-id' '#{hook_pane}'"
        );
    }

    #[test]
    fn chrome_refresh_all_skips_target_host_sessions() {
        let chrome = ManagedSessionRecord {
            address: ManagedSessionAddress::local_tmux("wa-1", "session-1"),
            workspace_dir: Some(PathBuf::from("/tmp/demo")),
            workspace_key: None,
            session_role: Some(WorkspaceSessionRole::WorkspaceChrome),
            attached_clients: 1,
            window_count: 1,
            command_name: Some("bash".to_string()),
            current_path: Some(PathBuf::from("/tmp/demo")),
            task_state: ManagedSessionTaskState::Input,
        };
        let target = ManagedSessionRecord {
            address: ManagedSessionAddress::local_tmux("wa-1", "session-2"),
            workspace_dir: Some(PathBuf::from("/tmp/demo")),
            workspace_key: None,
            session_role: Some(WorkspaceSessionRole::TargetHost),
            attached_clients: 0,
            window_count: 1,
            command_name: Some("bash".to_string()),
            current_path: Some(PathBuf::from("/tmp/demo")),
            task_state: ManagedSessionTaskState::Running,
        };

        assert!(should_refresh_workspace_chrome(&chrome));
        assert!(!should_refresh_workspace_chrome(&target));
    }

    fn workspace() -> TmuxWorkspaceHandle {
        TmuxWorkspaceHandle {
            workspace_id: WorkspaceInstanceId::new("session-1"),
            socket_name: TmuxSocketName::new("wa-1"),
            session_name: TmuxSessionName::new("session-1"),
        }
    }
}
