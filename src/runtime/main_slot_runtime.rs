use crate::application::layout_service::{FOOTER_PANE_TITLE, SIDEBAR_PANE_TITLE};
use crate::application::session_service::SessionService;
use crate::cli::{ActivateTargetCommand, MainPaneDiedCommand, NewTargetCommand};
use crate::domain::session_catalog::ManagedSessionRecord;
use crate::domain::workspace::WorkspaceInstanceConfig;
use crate::domain::workspace::WorkspaceSessionRole;
use crate::infra::tmux::{
    EmbeddedTmuxBackend, TmuxError, TmuxGateway, TmuxLayoutGateway, TmuxPaneId, TmuxProgram,
    TmuxSessionName, TmuxSocketName, TmuxWorkspaceHandle,
};
use crate::lifecycle::LifecycleError;
use crate::runtime::target_host_runtime::TargetHostRuntime;
use crate::runtime::workspace_layout_runtime::WorkspaceLayoutRuntime;
use crate::terminal::TerminalRuntime;
use std::io;
use std::path::Path;
use std::path::PathBuf;

const WAITAGENT_MAIN_PANE_OPTION: &str = "@waitagent_main_pane_id";
const WAITAGENT_ACTIVE_TARGET_OPTION: &str = "@waitagent_active_target";

pub struct MainSlotRuntime {
    backend: EmbeddedTmuxBackend,
    target_host_runtime: TargetHostRuntime,
    layout_runtime: WorkspaceLayoutRuntime,
    session_service: SessionService<EmbeddedTmuxBackend>,
    current_executable: PathBuf,
}

impl MainSlotRuntime {
    pub fn new(
        backend: EmbeddedTmuxBackend,
        target_host_runtime: TargetHostRuntime,
        layout_runtime: WorkspaceLayoutRuntime,
        session_service: SessionService<EmbeddedTmuxBackend>,
        current_executable: PathBuf,
    ) -> Self {
        Self {
            backend,
            target_host_runtime,
            layout_runtime,
            session_service,
            current_executable,
        }
    }

    pub fn run_activate_target(
        &self,
        command: ActivateTargetCommand,
    ) -> Result<(), LifecycleError> {
        let current_workspace = self.current_workspace(&command)?;
        let session = self
            .session_service
            .find_session(&command.target)
            .map_err(main_slot_error)?
            .ok_or_else(|| {
                LifecycleError::Protocol(format!("unknown tmux target `{}`", command.target))
            })?;

        if session.address.server_id() == command.current_socket_name
            && session.address.session_id() == command.current_session_name
        {
            self.focus_target_main_pane(&session.address)?;
            return Ok(());
        }

        if session.address.server_id() == command.current_socket_name {
            return self.activate_target_in_workspace(&current_workspace, &session);
        }

        self.backend
            .run_socket_command(
                &TmuxSocketName::new(&command.current_socket_name),
                &[
                    "detach-client".to_string(),
                    "-E".to_string(),
                    format!(
                        "{} attach {}",
                        shell_escape(self.current_executable.to_string_lossy().as_ref()),
                        shell_escape(&command.target)
                    ),
                ],
            )
            .map_err(main_slot_error)
    }

    pub fn run_new_target(&self, command: NewTargetCommand) -> Result<(), LifecycleError> {
        let current_workspace = self.current_workspace_from_names(
            &command.current_socket_name,
            &command.current_session_name,
        )?;
        let terminal_size = TerminalRuntime::stdio().current_size_or_default();
        let (rows, cols) = if terminal_size.rows > 1 && terminal_size.cols > 1 {
            (Some(terminal_size.rows), Some(terminal_size.cols))
        } else {
            (None, None)
        };
        let config = WorkspaceInstanceConfig::for_new_target_on_socket_with_size(
            &current_workspace.workspace_dir,
            &command.current_socket_name,
            rows,
            cols,
        );
        let target_host = self
            .target_host_runtime
            .ensure_target_host(config)
            .map_err(main_slot_error)?;
        let target = self.resolve_session_on_socket(
            &target_host.workspace_handle.socket_name,
            target_host.workspace_handle.session_name.as_str(),
        )?;
        self.activate_target_in_workspace(&current_workspace, &target)
    }

    pub fn run_main_pane_died(&self, command: MainPaneDiedCommand) -> Result<(), LifecycleError> {
        let current_workspace =
            self.current_workspace_from_names(&command.socket_name, &command.session_name)?;
        let workspace = workspace_handle(&command.socket_name, &command.session_name);
        let Some(main_pane_id) = self
            .backend
            .show_session_option(&workspace, WAITAGENT_MAIN_PANE_OPTION)
            .map_err(main_slot_error)?
        else {
            return Ok(());
        };
        if command.pane_id != main_pane_id {
            return Ok(());
        }
        let sessions = self
            .session_service
            .list_sessions_on_socket(&TmuxSocketName::new(&command.socket_name))
            .map_err(main_slot_error)?;
        let active_target = self.active_target(&workspace)?;
        let next_target =
            next_target_host_session(&sessions, &command.socket_name, active_target.as_deref());

        match next_target {
            Some(target) => {
                self.activate_target_after_main_pane_exit(&current_workspace, &target)?;
                self.close_target_session_identity(active_target.as_deref())?;
                self.layout_runtime
                    .refresh_workspace_chrome(&workspace, &current_workspace.workspace_dir)?;
                Ok(())
            }
            None => {
                self.close_target_session_identity(active_target.as_deref())?;
                self.layout_runtime
                    .run_close_session(crate::cli::CloseSessionCommand {
                        socket_name: command.socket_name,
                        session_name: command.session_name,
                    })
            }
        }
    }

    fn focus_target_main_pane(
        &self,
        address: &crate::domain::session_catalog::ManagedSessionAddress,
    ) -> Result<(), LifecycleError> {
        let workspace = TmuxWorkspaceHandle {
            workspace_id: crate::domain::workspace::WorkspaceInstanceId::new(address.session_id()),
            socket_name: TmuxSocketName::new(address.server_id()),
            session_name: TmuxSessionName::new(address.session_id()),
        };
        let Some(main_pane_id) = self
            .backend
            .show_session_option(&workspace, WAITAGENT_MAIN_PANE_OPTION)
            .map_err(main_slot_error)?
            .or_else(|| self.infer_target_main_pane(&workspace))
        else {
            return Ok(());
        };

        self.backend
            .select_pane(&workspace, &TmuxPaneId::new(main_pane_id))
            .map_err(main_slot_error)
    }

    fn activate_target_in_workspace(
        &self,
        current_workspace: &CurrentWorkspace,
        target: &crate::domain::session_catalog::ManagedSessionRecord,
    ) -> Result<(), LifecycleError> {
        if target.session_role != Some(WorkspaceSessionRole::TargetHost) {
            return Err(LifecycleError::Protocol(format!(
                "target `{}` is not a target host session",
                target.address.qualified_target()
            )));
        }

        let workspace = workspace_handle(
            &current_workspace.socket_name,
            &current_workspace.session_name,
        );
        if self.active_target(&workspace)?.as_deref()
            == Some(target.address.qualified_target().as_str())
        {
            self.layout_runtime
                .sync_main_slot_bindings(&workspace, &current_workspace.workspace_dir)?;
            return Ok(());
        }

        let workspace_main_pane = self.workspace_main_pane(&workspace)?;

        if let Some(active_target) = self.active_target(&workspace)? {
            if self
                .session_service
                .find_session(&active_target)
                .map_err(main_slot_error)?
                .is_some()
            {
                self.restore_active_target_to_host(
                    &workspace,
                    &active_target,
                    &workspace_main_pane,
                )?;
            }
        } else {
            self.capture_embedded_main_into_target_host(
                &workspace,
                &current_workspace.workspace_dir,
                &workspace_main_pane,
            )?;
        }

        let target_main_pane = self.target_main_pane(target)?;
        self.backend
            .swap_panes(&workspace, &target_main_pane, &workspace_main_pane)
            .map_err(main_slot_error)?;
        self.set_active_target(&workspace, Some(&target.address.qualified_target()))?;
        self.layout_runtime
            .sync_main_slot_bindings(&workspace, &current_workspace.workspace_dir)?;
        Ok(())
    }

    fn activate_target_after_main_pane_exit(
        &self,
        current_workspace: &CurrentWorkspace,
        target: &crate::domain::session_catalog::ManagedSessionRecord,
    ) -> Result<(), LifecycleError> {
        let workspace = workspace_handle(
            &current_workspace.socket_name,
            &current_workspace.session_name,
        );
        let workspace_main_pane = self.workspace_main_pane(&workspace)?;
        self.backend
            .respawn_pane(
                &workspace,
                &workspace_main_pane,
                &workspace_host_program(&current_workspace.workspace_dir),
            )
            .map_err(main_slot_error)?;
        let target_main_pane = self.target_main_pane(target)?;
        self.backend
            .swap_panes(&workspace, &target_main_pane, &workspace_main_pane)
            .map_err(main_slot_error)?;
        self.set_active_target(&workspace, Some(&target.address.qualified_target()))?;
        self.layout_runtime
            .sync_main_slot_bindings(&workspace, &current_workspace.workspace_dir)?;
        self.layout_runtime
            .refresh_workspace_chrome(&workspace, &current_workspace.workspace_dir)?;
        Ok(())
    }

    fn restore_active_target_to_host(
        &self,
        workspace: &TmuxWorkspaceHandle,
        active_target: &str,
        workspace_main_pane: &TmuxPaneId,
    ) -> Result<(), LifecycleError> {
        let active_session = self
            .session_service
            .find_session(active_target)
            .map_err(main_slot_error)?
            .ok_or_else(|| {
                LifecycleError::Protocol(format!(
                    "active target `{active_target}` could not be resolved"
                ))
            })?;
        let active_host_pane = self.target_main_pane(&active_session)?;
        self.backend
            .swap_panes(workspace, &active_host_pane, workspace_main_pane)
            .map_err(main_slot_error)
    }

    fn capture_embedded_main_into_target_host(
        &self,
        workspace: &TmuxWorkspaceHandle,
        workspace_dir: &Path,
        workspace_main_pane: &TmuxPaneId,
    ) -> Result<(), LifecycleError> {
        let terminal_size = TerminalRuntime::stdio().current_size_or_default();
        let (rows, cols) = if terminal_size.rows > 1 && terminal_size.cols > 1 {
            (Some(terminal_size.rows), Some(terminal_size.cols))
        } else {
            (None, None)
        };
        let host = self
            .target_host_runtime
            .ensure_target_host(WorkspaceInstanceConfig::for_new_target_on_socket_with_size(
                workspace_dir,
                workspace.socket_name.as_str(),
                rows,
                cols,
            ))
            .map_err(main_slot_error)?;
        let host_session = self.resolve_session_on_socket(
            &host.workspace_handle.socket_name,
            host.workspace_handle.session_name.as_str(),
        )?;
        let host_main_pane = self.target_main_pane(&host_session)?;
        self.backend
            .swap_panes(workspace, &host_main_pane, workspace_main_pane)
            .map_err(main_slot_error)
    }

    fn target_main_pane(
        &self,
        session: &crate::domain::session_catalog::ManagedSessionRecord,
    ) -> Result<TmuxPaneId, LifecycleError> {
        let workspace = TmuxWorkspaceHandle {
            workspace_id: crate::domain::workspace::WorkspaceInstanceId::new(
                session.address.session_id(),
            ),
            socket_name: TmuxSocketName::new(session.address.server_id()),
            session_name: TmuxSessionName::new(session.address.session_id()),
        };
        let pane_id = self.infer_target_main_pane(&workspace).ok_or_else(|| {
            LifecycleError::Protocol(format!(
                "target `{}` has no available main pane",
                session.address.qualified_target()
            ))
        })?;
        Ok(TmuxPaneId::new(pane_id))
    }

    fn workspace_main_pane(
        &self,
        workspace: &TmuxWorkspaceHandle,
    ) -> Result<TmuxPaneId, LifecycleError> {
        let pane = self
            .backend
            .show_session_option(workspace, WAITAGENT_MAIN_PANE_OPTION)
            .map_err(main_slot_error)?
            .or_else(|| self.infer_target_main_pane(workspace))
            .ok_or_else(|| {
                LifecycleError::Protocol(format!(
                    "workspace `{}` has no main pane",
                    workspace.session_name.as_str()
                ))
            })?;
        Ok(TmuxPaneId::new(pane))
    }

    fn active_target(
        &self,
        workspace: &TmuxWorkspaceHandle,
    ) -> Result<Option<String>, LifecycleError> {
        self.backend
            .show_session_option(workspace, WAITAGENT_ACTIVE_TARGET_OPTION)
            .map(|target| target.filter(|target| !target.is_empty()))
            .map_err(main_slot_error)
    }

    fn set_active_target(
        &self,
        workspace: &TmuxWorkspaceHandle,
        target: Option<&str>,
    ) -> Result<(), LifecycleError> {
        self.backend
            .set_session_option(
                workspace,
                WAITAGENT_ACTIVE_TARGET_OPTION,
                target.unwrap_or(""),
            )
            .map_err(main_slot_error)
    }

    fn resolve_session_on_socket(
        &self,
        socket_name: &TmuxSocketName,
        session_name: &str,
    ) -> Result<ManagedSessionRecord, LifecycleError> {
        self.session_service
            .list_sessions_on_socket(socket_name)
            .map_err(main_slot_error)?
            .into_iter()
            .find(|session| session.address.session_id() == session_name)
            .ok_or_else(|| {
                LifecycleError::Protocol(format!(
                    "session `{}` on socket `{}` could not be resolved",
                    session_name,
                    socket_name.as_str()
                ))
            })
    }

    fn close_target_session_identity(&self, target: Option<&str>) -> Result<(), LifecycleError> {
        let Some(target) = target else {
            return Ok(());
        };
        let Some(session) = self
            .session_service
            .find_session(target)
            .map_err(main_slot_error)?
        else {
            return Ok(());
        };
        self.backend
            .run_socket_command(
                &TmuxSocketName::new(session.address.server_id()),
                &[
                    "kill-session".to_string(),
                    "-t".to_string(),
                    session.address.session_id().to_string(),
                ],
            )
            .map_err(main_slot_error)
    }

    fn current_workspace(
        &self,
        command: &ActivateTargetCommand,
    ) -> Result<CurrentWorkspace, LifecycleError> {
        self.current_workspace_from_names(
            &command.current_socket_name,
            &command.current_session_name,
        )
    }

    fn current_workspace_from_names(
        &self,
        socket_name: &str,
        session_name: &str,
    ) -> Result<CurrentWorkspace, LifecycleError> {
        let current_target = format!("{socket_name}:{session_name}");
        let current_session = self
            .session_service
            .find_session(&current_target)
            .map_err(main_slot_error)?
            .ok_or_else(|| {
                LifecycleError::Protocol(format!("unknown current tmux target `{current_target}`"))
            })?;
        let workspace_dir = current_session.workspace_dir.ok_or_else(|| {
            LifecycleError::Protocol(format!(
                "current target `{current_target}` has no workspace directory metadata"
            ))
        })?;

        Ok(CurrentWorkspace {
            socket_name: socket_name.to_string(),
            session_name: session_name.to_string(),
            workspace_dir,
        })
    }

    fn infer_target_main_pane(&self, workspace: &TmuxWorkspaceHandle) -> Option<String> {
        let window = self.backend.current_window(workspace).ok()?;
        let panes = self.backend.list_panes(workspace, &window).ok()?;
        panes
            .iter()
            .find(|pane| pane.title != SIDEBAR_PANE_TITLE && pane.title != FOOTER_PANE_TITLE)
            .or_else(|| panes.first())
            .map(|pane| pane.pane_id.as_str().to_string())
    }
}

struct CurrentWorkspace {
    socket_name: String,
    session_name: String,
    workspace_dir: PathBuf,
}

fn workspace_handle(socket_name: &str, session_name: &str) -> TmuxWorkspaceHandle {
    TmuxWorkspaceHandle {
        workspace_id: crate::domain::workspace::WorkspaceInstanceId::new(session_name),
        socket_name: TmuxSocketName::new(socket_name),
        session_name: TmuxSessionName::new(session_name),
    }
}

fn next_target_host_session(
    sessions: &[ManagedSessionRecord],
    socket_name: &str,
    active_target: Option<&str>,
) -> Option<ManagedSessionRecord> {
    let same_socket_targets = sessions
        .iter()
        .filter(|session| session.address.server_id() == socket_name && session.is_target_host())
        .cloned()
        .collect::<Vec<_>>();

    let active_target = active_target.filter(|target| !target.is_empty());
    if let Some(active_target) = active_target {
        return same_socket_targets
            .into_iter()
            .find(|session| session.address.qualified_target() != active_target);
    }

    same_socket_targets.into_iter().next()
}

fn workspace_host_program(workspace_dir: &Path) -> TmuxProgram {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());
    TmuxProgram::new(shell).with_start_directory(workspace_dir)
}

fn main_slot_error(error: TmuxError) -> LifecycleError {
    LifecycleError::Io(
        "tmux-native main-slot command failed".to_string(),
        io::Error::new(io::ErrorKind::Other, error.to_string()),
    )
}

fn shell_escape(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

#[cfg(test)]
mod tests {
    use super::next_target_host_session;
    use crate::domain::session_catalog::{
        ManagedSessionAddress, ManagedSessionRecord, ManagedSessionTaskState,
    };
    use crate::domain::workspace::WorkspaceSessionRole;
    use std::path::PathBuf;

    #[test]
    fn next_target_host_session_prefers_another_target_on_same_socket() {
        let sessions = vec![
            session("wa-1", "workspace", WorkspaceSessionRole::WorkspaceChrome),
            session("wa-1", "target-a", WorkspaceSessionRole::TargetHost),
            session("wa-1", "target-b", WorkspaceSessionRole::TargetHost),
            session("wa-2", "target-c", WorkspaceSessionRole::TargetHost),
        ];

        let next = next_target_host_session(&sessions, "wa-1", Some("wa-1:target-a"))
            .expect("fallback target should exist");

        assert_eq!(next.address.qualified_target(), "wa-1:target-b");
    }

    #[test]
    fn next_target_host_session_returns_none_without_same_socket_target_hosts() {
        let sessions = vec![session(
            "wa-1",
            "workspace",
            WorkspaceSessionRole::WorkspaceChrome,
        )];

        assert!(next_target_host_session(&sessions, "wa-1", Some("wa-1:target-a")).is_none());
    }

    #[test]
    fn next_target_host_session_returns_none_when_only_active_target_remains() {
        let sessions = vec![
            session("wa-1", "workspace", WorkspaceSessionRole::WorkspaceChrome),
            session("wa-1", "target-a", WorkspaceSessionRole::TargetHost),
        ];

        assert!(next_target_host_session(&sessions, "wa-1", Some("wa-1:target-a")).is_none());
    }

    #[test]
    fn next_target_host_session_returns_first_target_without_active_target() {
        let sessions = vec![
            session("wa-1", "workspace", WorkspaceSessionRole::WorkspaceChrome),
            session("wa-1", "target-a", WorkspaceSessionRole::TargetHost),
            session("wa-1", "target-b", WorkspaceSessionRole::TargetHost),
        ];

        let next =
            next_target_host_session(&sessions, "wa-1", None).expect("a target should exist");

        assert_eq!(next.address.qualified_target(), "wa-1:target-a");
    }

    fn session(socket: &str, session: &str, role: WorkspaceSessionRole) -> ManagedSessionRecord {
        ManagedSessionRecord {
            address: ManagedSessionAddress::local_tmux(socket, session),
            workspace_dir: Some(PathBuf::from("/tmp/demo")),
            workspace_key: None,
            session_role: Some(role),
            attached_clients: 1,
            window_count: 1,
            command_name: Some("bash".to_string()),
            current_path: Some(PathBuf::from("/tmp/demo")),
            task_state: ManagedSessionTaskState::Input,
        }
    }
}
