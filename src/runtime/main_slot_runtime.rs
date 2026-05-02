use crate::application::layout_service::{FOOTER_PANE_TITLE, SIDEBAR_PANE_TITLE};
use crate::application::target_registry_service::{
    DefaultTargetCatalogGateway, TargetRegistryService,
};
use crate::cli::{prepend_global_network_args, RemoteNetworkConfig};
use crate::cli::{ActivateTargetCommand, MainPaneDiedCommand, NewTargetCommand};
use crate::domain::session_catalog::{ManagedSessionRecord, SessionTransport};
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
    target_registry: TargetRegistryService<DefaultTargetCatalogGateway>,
    current_executable: PathBuf,
    network: RemoteNetworkConfig,
}

impl MainSlotRuntime {
    pub fn new(
        backend: EmbeddedTmuxBackend,
        target_host_runtime: TargetHostRuntime,
        layout_runtime: WorkspaceLayoutRuntime,
        target_registry: TargetRegistryService<DefaultTargetCatalogGateway>,
        current_executable: PathBuf,
        network: RemoteNetworkConfig,
    ) -> Self {
        Self {
            backend,
            target_host_runtime,
            layout_runtime,
            target_registry,
            current_executable,
            network,
        }
    }

    pub fn run_activate_target(
        &self,
        command: ActivateTargetCommand,
    ) -> Result<(), LifecycleError> {
        let current_workspace = self.current_workspace(&command)?;
        let current_socket = TmuxSocketName::new(&command.current_socket_name);
        let session = if target_socket_name(&command.target)
            == Some(command.current_socket_name.as_str())
        {
            self.find_session_matching_on_socket(&current_socket, &command.target)?
        } else {
            self.target_registry
                .find_target(&command.target)
                .map_err(main_slot_error)?
        }
        .ok_or_else(|| LifecycleError::Protocol(format!("unknown target `{}`", command.target)))?;

        match session.address.transport() {
            SessionTransport::LocalTmux => {}
            SessionTransport::RemotePeer => {
                return self.activate_remote_target_in_workspace(&current_workspace, &session);
            }
        }

        if session.address.server_id() == command.current_socket_name
            && session.address.session_id() == command.current_session_name
        {
            self.focus_target_main_pane(&session.address)?;
            return Ok(());
        }

        if session.address.server_id() == command.current_socket_name {
            return self.activate_target_in_workspace(&current_workspace, &session);
        }

        Err(LifecycleError::Protocol(format!(
            "target `{}` is outside the current workspace socket `{}`",
            command.target, command.current_socket_name
        )))
    }

    pub fn run_new_target(&self, command: NewTargetCommand) -> Result<(), LifecycleError> {
        let current_workspace = self.current_workspace_from_names(
            &command.current_socket_name,
            &command.current_session_name,
        )?;
        let (rows, cols) = current_terminal_target_size();
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

    pub fn ensure_initial_target_materialized(
        &self,
        workspace: &TmuxWorkspaceHandle,
        workspace_dir: &Path,
    ) -> Result<(), LifecycleError> {
        if self.active_target(workspace)?.is_some() {
            self.layout_runtime
                .sync_main_slot_bindings(workspace, workspace_dir)?;
            return Ok(());
        }

        let current_workspace = CurrentWorkspace {
            socket_name: workspace.socket_name.as_str().to_string(),
            session_name: workspace.session_name.as_str().to_string(),
            workspace_dir: workspace_dir.to_path_buf(),
        };
        let sessions = self
            .target_registry
            .list_targets_on_authority(workspace.socket_name.as_str())
            .map_err(main_slot_error)?;

        if let Some(existing_target) = sessions.iter().find(|session| session.is_target_host()) {
            self.activate_target_in_workspace(&current_workspace, existing_target)?;
            self.layout_runtime
                .refresh_workspace_chrome(workspace, workspace_dir)?;
            return Ok(());
        }

        let (rows, cols) = current_terminal_target_size();
        let target_host = self
            .target_host_runtime
            .ensure_target_host(WorkspaceInstanceConfig::for_new_target_on_socket_with_size(
                workspace_dir,
                workspace.socket_name.as_str(),
                rows,
                cols,
            ))
            .map_err(main_slot_error)?;
        let target = self.resolve_session_on_socket(
            &target_host.workspace_handle.socket_name,
            target_host.workspace_handle.session_name.as_str(),
        )?;
        let workspace_main_pane = self.workspace_main_pane(workspace)?;
        let target_main_pane = self.target_main_pane(&target)?;
        self.backend
            .swap_panes(workspace, &target_main_pane, &workspace_main_pane)
            .map_err(main_slot_error)?;
        self.set_workspace_main_pane(workspace, &target_main_pane)?;
        self.set_active_target(workspace, Some(&target.address.qualified_target()))?;
        self.layout_runtime
            .sync_main_slot_bindings(workspace, workspace_dir)?;
        self.layout_runtime
            .refresh_workspace_chrome(workspace, workspace_dir)
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
            .target_registry
            .list_targets_on_authority(&command.socket_name)
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

        let mut workspace_main_pane = self.workspace_main_pane(&workspace)?;

        if let Some(active_target) = self.active_target(&workspace)? {
            if let Some(active_session) =
                self.find_session_matching_on_socket(&workspace.socket_name, &active_target)?
            {
                // tmux moves the source pane into the destination slot, so the visible
                // workspace main-pane identity becomes `active_host_pane` after restore.
                workspace_main_pane = self.restore_active_target_to_host(
                    &workspace,
                    &active_session,
                    &workspace_main_pane,
                )?;
            } else if self.remote_target_record(&active_target)?.is_some() {
                self.backend
                    .respawn_pane(
                        &workspace,
                        &workspace_main_pane,
                        &workspace_host_program(&current_workspace.workspace_dir),
                    )
                    .map_err(main_slot_error)?;
            }
        }

        let target_main_pane = self.target_main_pane(target)?;
        self.backend
            .swap_panes(&workspace, &target_main_pane, &workspace_main_pane)
            .map_err(main_slot_error)?;
        self.set_workspace_main_pane(&workspace, &target_main_pane)?;
        self.set_active_target(&workspace, Some(&target.address.qualified_target()))?;
        self.layout_runtime
            .sync_main_slot_bindings(&workspace, &current_workspace.workspace_dir)?;
        self.layout_runtime
            .refresh_workspace_chrome(&workspace, &current_workspace.workspace_dir)
    }

    fn activate_remote_target_in_workspace(
        &self,
        current_workspace: &CurrentWorkspace,
        target: &crate::domain::session_catalog::ManagedSessionRecord,
    ) -> Result<(), LifecycleError> {
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

        let mut workspace_main_pane = self.workspace_main_pane(&workspace)?;
        if let Some(active_target) = self.active_target(&workspace)? {
            if let Some(active_session) =
                self.find_session_matching_on_socket(&workspace.socket_name, &active_target)?
            {
                workspace_main_pane = self.restore_active_target_to_host(
                    &workspace,
                    &active_session,
                    &workspace_main_pane,
                )?;
            }
        }

        self.backend
            .respawn_pane(
                &workspace,
                &workspace_main_pane,
                &remote_main_slot_program(
                    &self.current_executable,
                    current_workspace,
                    &target.address.qualified_target(),
                    &self.network,
                ),
            )
            .map_err(main_slot_error)?;
        self.set_workspace_main_pane(&workspace, &workspace_main_pane)?;
        self.set_active_target(&workspace, Some(&target.address.qualified_target()))?;
        self.layout_runtime
            .sync_main_slot_bindings(&workspace, &current_workspace.workspace_dir)?;
        self.layout_runtime
            .refresh_workspace_chrome(&workspace, &current_workspace.workspace_dir)
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
        self.set_workspace_main_pane(&workspace, &target_main_pane)?;
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
        active_session: &crate::domain::session_catalog::ManagedSessionRecord,
        workspace_main_pane: &TmuxPaneId,
    ) -> Result<TmuxPaneId, LifecycleError> {
        let active_host_pane = self.target_main_pane(active_session)?;
        self.backend
            .swap_panes(workspace, &active_host_pane, workspace_main_pane)
            .map_err(main_slot_error)?;
        Ok(active_host_pane)
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
        let configured_pane = self
            .backend
            .show_session_option(workspace, WAITAGENT_MAIN_PANE_OPTION)
            .map_err(main_slot_error)?
            .filter(|pane| self.pane_is_live(workspace, pane));
        let pane = configured_pane
            .or_else(|| self.infer_target_main_pane(workspace))
            .ok_or_else(|| {
                LifecycleError::Protocol(format!(
                    "workspace `{}` has no main pane",
                    workspace.session_name.as_str()
                ))
            })?;
        let pane = TmuxPaneId::new(pane);
        self.set_workspace_main_pane(workspace, &pane)?;
        Ok(pane)
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

    fn set_workspace_main_pane(
        &self,
        workspace: &TmuxWorkspaceHandle,
        pane: &TmuxPaneId,
    ) -> Result<(), LifecycleError> {
        self.backend
            .set_session_option(workspace, WAITAGENT_MAIN_PANE_OPTION, pane.as_str())
            .map_err(main_slot_error)
    }

    fn resolve_session_on_socket(
        &self,
        socket_name: &TmuxSocketName,
        session_name: &str,
    ) -> Result<ManagedSessionRecord, LifecycleError> {
        self.target_registry
            .resolve_target_on_authority_session(socket_name.as_str(), session_name)
            .map_err(main_slot_error)?
            .ok_or_else(|| {
                LifecycleError::Protocol(format!(
                    "session `{}` on socket `{}` could not be resolved",
                    session_name,
                    socket_name.as_str()
                ))
            })
    }

    fn close_target_session_identity(&self, target: Option<&str>) -> Result<(), LifecycleError> {
        self.target_host_runtime
            .close_target_session_identity(target)
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
        let current_session =
            self.resolve_session_on_socket(&TmuxSocketName::new(socket_name), session_name)?;
        let workspace_dir = current_session.workspace_dir.ok_or_else(|| {
            LifecycleError::Protocol(format!(
                "current target `{socket_name}:{session_name}` has no workspace directory metadata"
            ))
        })?;

        Ok(CurrentWorkspace {
            socket_name: socket_name.to_string(),
            session_name: session_name.to_string(),
            workspace_dir,
        })
    }

    fn remote_target_record(
        &self,
        target: &str,
    ) -> Result<Option<ManagedSessionRecord>, LifecycleError> {
        Ok(self
            .target_registry
            .find_target(target)
            .map_err(main_slot_error)?
            .filter(|session| session.address.transport() == &SessionTransport::RemotePeer))
    }

    fn infer_target_main_pane(&self, workspace: &TmuxWorkspaceHandle) -> Option<String> {
        let window = self.backend.current_window(workspace).ok()?;
        let panes = self.backend.list_panes(workspace, &window).ok()?;
        panes
            .iter()
            .find(|pane| {
                !pane.is_dead && pane.title != SIDEBAR_PANE_TITLE && pane.title != FOOTER_PANE_TITLE
            })
            .or_else(|| panes.iter().find(|pane| !pane.is_dead))
            .map(|pane| pane.pane_id.as_str().to_string())
    }

    fn pane_is_live(&self, workspace: &TmuxWorkspaceHandle, pane_id: &str) -> bool {
        let Ok(window) = self.backend.current_window(workspace) else {
            return false;
        };
        let Ok(panes) = self.backend.list_panes(workspace, &window) else {
            return false;
        };
        panes
            .iter()
            .any(|pane| pane.pane_id.as_str() == pane_id && !pane.is_dead)
    }

    fn find_session_matching_on_socket(
        &self,
        socket_name: &TmuxSocketName,
        target: &str,
    ) -> Result<Option<ManagedSessionRecord>, LifecycleError> {
        self.target_registry
            .find_target_on_authority(socket_name.as_str(), target)
            .map_err(main_slot_error)
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

fn target_socket_name(target: &str) -> Option<&str> {
    split_qualified_target(target).map(|(socket_name, _)| socket_name)
}

fn split_qualified_target(target: &str) -> Option<(&str, &str)> {
    let (socket_name, session_name) = target.split_once(':')?;
    if socket_name.is_empty() || session_name.is_empty() {
        return None;
    }
    Some((socket_name, session_name))
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

fn remote_main_slot_program(
    executable: &Path,
    current_workspace: &CurrentWorkspace,
    target: &str,
    network: &RemoteNetworkConfig,
) -> TmuxProgram {
    TmuxProgram::new(executable.display().to_string())
        .with_args(prepend_global_network_args(
            vec![
                "__remote-main-slot".to_string(),
                "--socket-name".to_string(),
                current_workspace.socket_name.clone(),
                "--session-name".to_string(),
                current_workspace.session_name.clone(),
                "--target".to_string(),
                target.to_string(),
            ],
            network,
        ))
        .with_start_directory(&current_workspace.workspace_dir)
}

fn current_terminal_target_size() -> (Option<u16>, Option<u16>) {
    let terminal_size = TerminalRuntime::stdio().current_size_or_default();
    if terminal_size.rows > 1 && terminal_size.cols > 1 {
        (Some(terminal_size.rows), Some(terminal_size.cols))
    } else {
        (None, None)
    }
}

fn main_slot_error(error: TmuxError) -> LifecycleError {
    LifecycleError::Io(
        "tmux-native main-slot command failed".to_string(),
        io::Error::new(io::ErrorKind::Other, error.to_string()),
    )
}

#[cfg(test)]
mod tests {
    use super::{
        next_target_host_session, remote_main_slot_program, split_qualified_target,
        target_socket_name, CurrentWorkspace, MainSlotRuntime, FOOTER_PANE_TITLE,
        SIDEBAR_PANE_TITLE, WAITAGENT_ACTIVE_TARGET_OPTION,
    };
    use crate::application::target_registry_service::{
        DefaultTargetCatalogGateway, TargetRegistryService,
    };
    use crate::application::workspace_service::WorkspaceService;
    use crate::cli::ActivateTargetCommand;
    use crate::cli::RemoteNetworkConfig;
    use crate::domain::session_catalog::{
        ManagedSessionAddress, ManagedSessionRecord, ManagedSessionTaskState, SessionAvailability,
    };
    use crate::domain::workspace::{
        WorkspaceInstanceConfig, WorkspaceInstanceId, WorkspaceSessionRole,
    };
    use crate::infra::tmux::{
        EmbeddedTmuxBackend, TmuxGateway, TmuxLayoutGateway, TmuxSessionName, TmuxSocketName,
        TmuxWorkspaceHandle,
    };
    use crate::runtime::remote_runtime_owner_runtime::RemoteRuntimeOwnerRuntime;
    use crate::runtime::target_host_runtime::TargetHostRuntime;
    use crate::runtime::workspace_entry_runtime::WorkspaceEntryRuntime;
    use crate::runtime::workspace_layout_runtime::WorkspaceLayoutRuntime;
    use crate::runtime::workspace_runtime::WorkspaceRuntime;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::thread;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
    fn next_target_host_session_ignores_remote_targets_when_local_target_host_exits() {
        let sessions = vec![
            session("wa-1", "workspace", WorkspaceSessionRole::WorkspaceChrome),
            remote_session("192.168.31.18", "pty1"),
        ];

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

    #[test]
    fn split_qualified_target_parses_socket_and_session_name() {
        assert_eq!(
            split_qualified_target("wa-1:target-a"),
            Some(("wa-1", "target-a"))
        );
        assert_eq!(target_socket_name("wa-1:target-a"), Some("wa-1"));
    }

    #[test]
    fn split_qualified_target_rejects_missing_parts() {
        assert_eq!(split_qualified_target("wa-1"), None);
        assert_eq!(split_qualified_target("wa-1:"), None);
        assert_eq!(split_qualified_target(":target-a"), None);
    }

    #[test]
    fn remote_main_slot_program_targets_workspace_and_remote_target() {
        let workspace = CurrentWorkspace {
            socket_name: "wa-1".to_string(),
            session_name: "workspace-1".to_string(),
            workspace_dir: PathBuf::from("/tmp/demo"),
        };

        let program = remote_main_slot_program(
            std::path::Path::new("/tmp/waitagent"),
            &workspace,
            "peer-a:shell-1",
            &RemoteNetworkConfig::default(),
        );

        assert_eq!(program.program, "/tmp/waitagent");
        assert_eq!(
            program.args,
            vec![
                "--port".to_string(),
                "7474".to_string(),
                "__remote-main-slot".to_string(),
                "--socket-name".to_string(),
                "wa-1".to_string(),
                "--session-name".to_string(),
                "workspace-1".to_string(),
                "--target".to_string(),
                "peer-a:shell-1".to_string(),
            ]
        );
        assert_eq!(program.start_directory, Some(PathBuf::from("/tmp/demo")));
    }

    #[test]
    fn activating_remote_target_respawns_workspace_main_pane_not_detached_target_host() {
        let backend = EmbeddedTmuxBackend::from_build_env()
            .expect("vendored tmux backend should discover build env");
        let workspace_config = unique_workspace_config("remote-main-slot");
        let workspace_dir = workspace_config.workspace_dir.clone();
        let waitagent_executable = waitagent_test_executable();
        let entry_runtime = WorkspaceEntryRuntime::new(
            WorkspaceRuntime::new(WorkspaceService::new(backend.clone())),
            WorkspaceLayoutRuntime::new_for_tests(
                backend.clone(),
                waitagent_executable.clone(),
                RemoteNetworkConfig::default(),
            )
            .expect("workspace layout runtime should build"),
        );
        let workspace = entry_runtime
            .bootstrap_workspace(&workspace_dir)
            .expect("workspace bootstrap should succeed");
        let target_host = backend
            .ensure_workspace(
                &WorkspaceInstanceConfig::for_new_target_on_socket_with_size(
                    &workspace_dir,
                    workspace.workspace_handle.socket_name.as_str(),
                    None,
                    None,
                ),
            )
            .expect("target host bootstrap should succeed");

        let runtime = MainSlotRuntime::new(
            backend.clone(),
            TargetHostRuntime::from_build_env(backend.clone())
                .expect("target host runtime should build"),
            WorkspaceLayoutRuntime::new_for_tests(
                backend.clone(),
                waitagent_executable.clone(),
                RemoteNetworkConfig::default(),
            )
            .expect("workspace layout runtime should build"),
            TargetRegistryService::new(
                DefaultTargetCatalogGateway::from_build_env_with_socket_name(
                    workspace.workspace_handle.socket_name.as_str(),
                )
                .expect("target catalog gateway should build"),
            ),
            waitagent_executable.clone(),
            RemoteNetworkConfig::default(),
        );

        let local_target = format!(
            "{}:{}",
            workspace.workspace_handle.socket_name.as_str(),
            target_host.session_name.as_str()
        );
        runtime
            .run_activate_target(ActivateTargetCommand {
                current_socket_name: workspace.workspace_handle.socket_name.as_str().to_string(),
                current_session_name: workspace.workspace_handle.session_name.as_str().to_string(),
                target: local_target.clone(),
            })
            .expect("local target activation should succeed");

        let remote_runtime_owner = RemoteRuntimeOwnerRuntime::new_for_tests(
            waitagent_executable.clone(),
            RemoteNetworkConfig::default(),
        );
        let remote_target = remote_session_with_selector(
            "peer-a",
            "remote-1",
            &local_target,
            ManagedSessionTaskState::Input,
        );
        remote_runtime_owner
            .upsert_session(
                workspace.workspace_handle.socket_name.as_str(),
                "peer-a",
                &remote_target,
            )
            .expect("remote target should be discoverable on workspace socket");

        runtime
            .run_activate_target(ActivateTargetCommand {
                current_socket_name: workspace.workspace_handle.socket_name.as_str().to_string(),
                current_session_name: workspace.workspace_handle.session_name.as_str().to_string(),
                target: remote_target.address.qualified_target(),
            })
            .expect("remote target activation should succeed");

        wait_for_condition(|| {
            let active_target = backend
                .show_session_option(&workspace.workspace_handle, WAITAGENT_ACTIVE_TARGET_OPTION)
                .expect("active target should read");
            active_target.as_deref() == Some(remote_target.address.qualified_target().as_str())
        });

        wait_for_condition(|| {
            workspace_main_pane_command(&backend, &workspace.workspace_handle).as_deref()
                == Some("waitagent")
        });

        let target_host_handle = TmuxWorkspaceHandle {
            workspace_id: WorkspaceInstanceId::new(target_host.session_name.as_str().to_string()),
            socket_name: TmuxSocketName::new(
                workspace.workspace_handle.socket_name.as_str().to_string(),
            ),
            session_name: TmuxSessionName::new(target_host.session_name.as_str().to_string()),
        };
        let target_host_command =
            workspace_main_pane_command(&backend, &target_host_handle).expect("target host pane");
        kill_server(&backend, &workspace.workspace_handle);
        let _ = fs::remove_dir_all(workspace_dir);

        assert_eq!(target_host_command, "bash");
    }

    fn session(socket: &str, session: &str, role: WorkspaceSessionRole) -> ManagedSessionRecord {
        ManagedSessionRecord {
            address: ManagedSessionAddress::local_tmux(socket, session),
            selector: Some(format!("{socket}:{session}")),
            availability: crate::domain::session_catalog::SessionAvailability::Online,
            workspace_dir: Some(PathBuf::from("/tmp/demo")),
            workspace_key: None,
            session_role: Some(role),
            opened_by: Vec::new(),
            attached_clients: 1,
            window_count: 1,
            command_name: Some("bash".to_string()),
            current_path: Some(PathBuf::from("/tmp/demo")),
            task_state: ManagedSessionTaskState::Input,
        }
    }

    fn remote_session(authority_id: &str, session_id: &str) -> ManagedSessionRecord {
        remote_session_with_selector(
            authority_id,
            session_id,
            &format!("{authority_id}:{session_id}"),
            ManagedSessionTaskState::Running,
        )
    }

    fn remote_session_with_selector(
        authority_id: &str,
        session_id: &str,
        selector: &str,
        task_state: ManagedSessionTaskState,
    ) -> ManagedSessionRecord {
        ManagedSessionRecord {
            address: ManagedSessionAddress::remote_peer(authority_id, session_id),
            selector: Some(selector.to_string()),
            availability: SessionAvailability::Online,
            workspace_dir: None,
            workspace_key: None,
            session_role: Some(WorkspaceSessionRole::TargetHost),
            opened_by: Vec::new(),
            attached_clients: 1,
            window_count: 1,
            command_name: Some("bash".to_string()),
            current_path: None,
            task_state,
        }
    }

    fn unique_workspace_config(prefix: &str) -> WorkspaceInstanceConfig {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after unix epoch")
            .as_nanos();
        let workspace_dir = std::env::temp_dir().join(format!("waitagent-{prefix}-{nonce:x}"));
        fs::create_dir_all(&workspace_dir)
            .expect("temporary workspace directory should be created");
        WorkspaceInstanceConfig {
            workspace_dir,
            workspace_key: format!("{prefix}-{nonce:x}"),
            socket_name: format!("wa-test-{nonce:x}"),
            session_name: format!("waitagent-test-{prefix}-{nonce:x}"),
            session_role: WorkspaceSessionRole::WorkspaceChrome,
            initial_rows: None,
            initial_cols: None,
        }
    }

    fn waitagent_test_executable() -> PathBuf {
        let current_exe = std::env::current_exe().expect("current test executable should exist");
        let executable = current_exe
            .parent()
            .and_then(Path::parent)
            .expect("test executable should live under target/debug/deps")
            .join(format!("waitagent{}", std::env::consts::EXE_SUFFIX));
        assert!(
            executable.exists(),
            "waitagent test executable should exist at {}",
            executable.display()
        );
        executable
    }

    fn workspace_main_pane_command(
        backend: &EmbeddedTmuxBackend,
        workspace: &TmuxWorkspaceHandle,
    ) -> Option<String> {
        let window = backend.current_window(workspace).ok()?;
        let panes = backend.list_panes(workspace, &window).ok()?;
        panes
            .into_iter()
            .find(|pane| {
                !pane.is_dead && pane.title != SIDEBAR_PANE_TITLE && pane.title != FOOTER_PANE_TITLE
            })
            .and_then(|pane| pane.current_command)
    }

    fn wait_for_condition(predicate: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if predicate() {
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
        assert!(predicate(), "condition should become true before timeout");
    }

    fn kill_server(backend: &EmbeddedTmuxBackend, workspace: &TmuxWorkspaceHandle) {
        let _ = backend.run_socket_command(
            &TmuxSocketName::new(workspace.socket_name.as_str().to_string()),
            &["kill-server".to_string()],
        );
    }
}
