use crate::application::layout_service::{FOOTER_PANE_TITLE, SIDEBAR_PANE_TITLE};
use crate::application::target_registry_service::{
    DefaultTargetCatalogGateway, TargetRegistryService,
};
use crate::cli::{prepend_global_network_args, RemoteNetworkConfig};
use crate::cli::{ActivateTargetCommand, MainPaneDiedCommand, NewTargetCommand};
use crate::domain::session_catalog::{ManagedSessionRecord, SessionTransport};
use crate::domain::workspace::WorkspaceSessionRole;
use crate::domain::workspace::{WorkspaceInstanceConfig, WorkspaceInstanceId};
use crate::infra::error_log::ERROR_LOG;
use crate::infra::tmux::{
    EmbeddedTmuxBackend, TmuxError, TmuxGateway, TmuxLayoutGateway, TmuxPaneId, TmuxProgram,
    TmuxSessionName, TmuxSocketName, TmuxSplitSize, TmuxWorkspaceHandle,
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
const WAITAGENT_SESSION_PANE_PREFIX: &str = "@waitagent_session_pane_";

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
        let socket_scoped_registry =
            self.target_registry_for_socket(command.current_socket_name.as_str())?;
        let session = if target_socket_name(&command.target)
            == Some(command.current_socket_name.as_str())
        {
            self.find_session_matching_on_socket(
                &socket_scoped_registry,
                &current_socket,
                &command.target,
            )?
        } else {
            socket_scoped_registry
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
        if let Some(active_target) = self.active_target(workspace)? {
            self.configure_main_pane_output_bridge_for_active_target(
                workspace,
                Some(active_target.as_str()),
            )?;
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
            .disable_main_pane_output_bridge(workspace)?;
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
        let recovery_pane = TmuxPaneId::new(main_pane_id);
        let active_target = self.active_target(&workspace)?;
        if self.active_target_is_remote(workspace.socket_name.as_str(), active_target.as_deref())? {
            self.fallback_after_remote_main_pane_exit(
                &current_workspace,
                &workspace,
                &recovery_pane,
                active_target,
            )?;
            return Ok(());
        }
        let sessions = self
            .target_registry
            .list_targets_on_authority(&command.socket_name)
            .map_err(main_slot_error)?;
        let next_target =
            next_target_host_session(&sessions, &command.socket_name, active_target.as_deref());

        match next_target {
            Some(target) => {
                self.activate_target_after_main_pane_exit(
                    &current_workspace,
                    &recovery_pane,
                    &target,
                )?;
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
                .disable_main_pane_output_bridge(&workspace)?;
            self.layout_runtime
                .sync_main_slot_bindings(&workspace, &current_workspace.workspace_dir)?;
            return Ok(());
        }

        let mut workspace_main_pane = self.workspace_main_pane(&workspace)?;

        if let Some(active_target) = self.active_target(&workspace)? {
            if let Some((_, target_session)) = split_qualified_target(&active_target)
                .filter(|(sock, _)| *sock == workspace.socket_name.as_str())
            {
                let target_workspace = TmuxWorkspaceHandle {
                    workspace_id: WorkspaceInstanceId::new(target_session),
                    socket_name: TmuxSocketName::new(workspace.socket_name.as_str()),
                    session_name: TmuxSessionName::new(target_session),
                };
                if let Some(active_host_pane_id) = self.infer_target_main_pane(&target_workspace) {
                    self.backend
                        .swap_panes(
                            &workspace,
                            &TmuxPaneId::new(active_host_pane_id.clone()),
                            &workspace_main_pane,
                        )
                        .map_err(main_slot_error)?;
                    workspace_main_pane = TmuxPaneId::new(active_host_pane_id);
                }
            } else if self
                .remote_target_record(workspace.socket_name.as_str(), &active_target)?
                .is_some()
            {
                self.cleanup_stale_isolation_pane(&workspace, &workspace_main_pane)?;
            }
        }

        let target_main_pane = self.target_main_pane(target)?;
        self.backend
            .swap_panes(&workspace, &target_main_pane, &workspace_main_pane)
            .map_err(main_slot_error)?;
        self.set_workspace_main_pane(&workspace, &target_main_pane)?;
        self.set_active_target(&workspace, Some(&target.address.qualified_target()))?;
        self.layout_runtime
            .disable_main_pane_output_bridge(&workspace)?;
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
        let target_id = target.address.id().as_str().to_string();
        let qualified_target = target.address.qualified_target();
        ERROR_LOG.log(format!(
            "[diag][{}] activate_remote_target_in_workspace: start",
            target_id
        ));

        let workspace = workspace_handle(
            &current_workspace.socket_name,
            &current_workspace.session_name,
        );
        if self.active_target(&workspace)?.as_deref() == Some(qualified_target.as_str()) {
            ERROR_LOG.log(format!(
                "[diag][{}] already active target, re-syncing bindings",
                target_id
            ));
            self.layout_runtime
                .disable_main_pane_output_bridge(&workspace)?;
            self.layout_runtime
                .sync_main_slot_bindings(&workspace, &current_workspace.workspace_dir)?;
            return Ok(());
        }

        let mut workspace_main_pane = self.workspace_main_pane(&workspace)?;
        ERROR_LOG.log(format!(
            "[diag][{}] workspace_main_pane={:?}",
            target_id, workspace_main_pane
        ));

        // If switching from a local target host on the same socket, swap its
        // pane back to its own position before swapping in the remote session.
        if let Some(active_target) = self.active_target(&workspace)? {
            ERROR_LOG.log(format!(
                "[diag][{}] active_target={:?}",
                target_id, active_target
            ));
            if let Some((_, target_session)) = split_qualified_target(&active_target)
                .filter(|(sock, _)| *sock == workspace.socket_name.as_str())
            {
                let target_workspace = TmuxWorkspaceHandle {
                    workspace_id: WorkspaceInstanceId::new(target_session),
                    socket_name: TmuxSocketName::new(workspace.socket_name.as_str()),
                    session_name: TmuxSessionName::new(target_session),
                };
                ERROR_LOG.log(format!(
                    "[diag][{}] swapping local host pane back for target_workspace={:?}",
                    target_id, target_workspace.session_name
                ));
                if let Some(active_host_pane_id) = self.infer_target_main_pane(&target_workspace) {
                    ERROR_LOG.log(format!(
                        "[diag][{}] swapping pane {:?} with workspace_main_pane {:?}",
                        target_id, active_host_pane_id, workspace_main_pane
                    ));
                    self.backend
                        .swap_panes(
                            &workspace,
                            &TmuxPaneId::new(active_host_pane_id.clone()),
                            &workspace_main_pane,
                        )
                        .map_err(main_slot_error)?;
                    workspace_main_pane = TmuxPaneId::new(active_host_pane_id);
                } else {
                    ERROR_LOG.log(format!(
                        "[diag][{}] infer_target_main_pane returned None, skipping host swap",
                        target_id
                    ));
                }
            }
        }

        // One-time migration: clean up stale isolation panes from old architecture
        self.cleanup_stale_isolation_pane(&workspace, &workspace_main_pane)?;
        ERROR_LOG.log(format!(
            "[diag][{}] after cleanup_stale_isolation_pane, workspace_main_pane={:?}",
            target_id, workspace_main_pane
        ));

        // Find or create a persistent per-session pane for this remote target
        let session_pane = match self.find_session_pane(&workspace, &qualified_target)? {
            Some(existing_pane) => {
                ERROR_LOG.log(format!(
                    "[diag][{}] found existing session_pane={:?}",
                    target_id, existing_pane
                ));
                existing_pane
            }
            None => {
                ERROR_LOG.log(format!(
                    "[diag][{}] creating new remote session pane",
                    target_id
                ));
                let new_pane = self.create_remote_session_pane(
                    &workspace,
                    &workspace_main_pane,
                    current_workspace,
                    target.address.id().as_str(),
                )?;
                ERROR_LOG.log(format!(
                    "[diag][{}] new remote session pane={:?}",
                    target_id, new_pane
                ));
                self.set_session_pane(&workspace, &qualified_target, &new_pane)?;
                new_pane
            }
        };

        // Swap the session pane into the display position
        ERROR_LOG.log(format!(
            "[diag][{}] swapping session_pane={:?} with workspace_main_pane={:?}",
            target_id, session_pane, workspace_main_pane
        ));
        self.backend
            .swap_panes(&workspace, &session_pane, &workspace_main_pane)
            .map_err(|e| {
                ERROR_LOG.log(format!("[diag][{}] swap_panes FAILED: {:?}", target_id, e));
                main_slot_error(e)
            })?;
        ERROR_LOG.log(format!(
            "[diag][{}] swap_panes succeeded, setting workspace main pane",
            target_id
        ));

        self.set_workspace_main_pane(&workspace, &session_pane)?;
        ERROR_LOG.log(format!(
            "[diag][{}] set_workspace_main_pane done, setting active_target",
            target_id
        ));

        self.set_active_target(&workspace, Some(&qualified_target))?;
        ERROR_LOG.log(format!(
            "[diag][{}] set_active_target done, disabling bridge",
            target_id
        ));

        self.layout_runtime
            .disable_main_pane_output_bridge(&workspace)?;
        ERROR_LOG.log(format!(
            "[diag][{}] bridge disabled, syncing main slot bindings",
            target_id
        ));

        self.layout_runtime
            .sync_main_slot_bindings(&workspace, &current_workspace.workspace_dir)?;
        ERROR_LOG.log(format!(
            "[diag][{}] bindings synced, refreshing chrome",
            target_id
        ));

        let result = self
            .layout_runtime
            .refresh_workspace_chrome(&workspace, &current_workspace.workspace_dir);
        ERROR_LOG.log(format!(
            "[diag][{}] refresh_workspace_chrome result={:?}",
            target_id, result
        ));
        result
    }

    fn activate_target_after_main_pane_exit(
        &self,
        current_workspace: &CurrentWorkspace,
        recovery_pane: &TmuxPaneId,
        target: &crate::domain::session_catalog::ManagedSessionRecord,
    ) -> Result<(), LifecycleError> {
        let workspace = workspace_handle(
            &current_workspace.socket_name,
            &current_workspace.session_name,
        );
        self.backend
            .respawn_pane(
                &workspace,
                recovery_pane,
                &workspace_host_program(&current_workspace.workspace_dir),
            )
            .map_err(main_slot_error)?;
        let target_main_pane = self.target_main_pane(target)?;
        self.backend
            .swap_panes(&workspace, &target_main_pane, recovery_pane)
            .map_err(main_slot_error)?;
        self.set_workspace_main_pane(&workspace, &target_main_pane)?;
        self.set_active_target(&workspace, Some(&target.address.qualified_target()))?;
        self.layout_runtime
            .disable_main_pane_output_bridge(&workspace)?;
        self.layout_runtime
            .sync_main_slot_bindings(&workspace, &current_workspace.workspace_dir)?;
        self.layout_runtime
            .refresh_workspace_chrome(&workspace, &current_workspace.workspace_dir)?;
        Ok(())
    }

    fn fallback_after_remote_main_pane_exit(
        &self,
        current_workspace: &CurrentWorkspace,
        workspace: &TmuxWorkspaceHandle,
        recovery_pane: &TmuxPaneId,
        active_target: Option<String>,
    ) -> Result<(), LifecycleError> {
        let sessions = self
            .target_registry
            .list_targets_on_authority(workspace.socket_name.as_str())
            .map_err(main_slot_error)?;
        let next_target = next_target_host_session(&sessions, workspace.socket_name.as_str(), None);
        match next_target {
            Some(target) => {
                self.activate_target_after_main_pane_exit(
                    current_workspace,
                    recovery_pane,
                    &target,
                )?;
                self.set_active_target(
                    workspace,
                    Some(target.address.qualified_target().as_str()),
                )?;
                self.layout_runtime
                    .disable_main_pane_output_bridge(workspace)?;
            }
            None => {
                self.backend
                    .respawn_pane(
                        workspace,
                        recovery_pane,
                        &workspace_host_program(&current_workspace.workspace_dir),
                    )
                    .map_err(main_slot_error)?;
                self.set_workspace_main_pane(workspace, recovery_pane)?;
                self.set_active_target(workspace, None)?;
                self.layout_runtime
                    .disable_main_pane_output_bridge(workspace)?;
                self.layout_runtime
                    .sync_main_slot_bindings(workspace, &current_workspace.workspace_dir)?;
                self.layout_runtime
                    .refresh_workspace_chrome(workspace, &current_workspace.workspace_dir)?;
            }
        }
        self.close_target_session_identity(active_target.as_deref())?;
        Ok(())
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
        self.target_registry_for_socket(socket_name.as_str())?
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
        let socket = TmuxSocketName::new(socket_name);
        let workspace_dir = self
            .backend
            .session_workspace_dir(&socket, session_name)
            .map_err(main_slot_error)?
            .ok_or_else(|| {
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
        socket_name: &str,
        target: &str,
    ) -> Result<Option<ManagedSessionRecord>, LifecycleError> {
        Ok(self
            .target_registry_for_socket(socket_name)?
            .find_target(target)
            .map_err(main_slot_error)?
            .filter(|session| session.address.transport() == &SessionTransport::RemotePeer))
    }

    fn active_target_is_remote(
        &self,
        socket_name: &str,
        target: Option<&str>,
    ) -> Result<bool, LifecycleError> {
        let Some(target) = target else {
            return Ok(false);
        };
        Ok(self.remote_target_record(socket_name, target)?.is_some())
    }

    fn session_pane_option_name(&self, qualified_target: &str) -> String {
        format!(
            "{}{}",
            WAITAGENT_SESSION_PANE_PREFIX,
            qualified_target.replace(':', ".")
        )
    }

    fn find_session_pane(
        &self,
        workspace: &TmuxWorkspaceHandle,
        qualified_target: &str,
    ) -> Result<Option<TmuxPaneId>, LifecycleError> {
        let option_name = self.session_pane_option_name(qualified_target);
        let pane_id_str = self
            .backend
            .show_session_option(workspace, &option_name)
            .map_err(main_slot_error)?;
        let Some(pane_id_str) = pane_id_str else {
            return Ok(None);
        };
        let pane_id = TmuxPaneId::new(pane_id_str);
        if self.pane_is_live(workspace, pane_id.as_str()) {
            Ok(Some(pane_id))
        } else {
            // Pane died — clean up the stale option so a new one is created
            self.backend
                .set_session_option(workspace, &option_name, "")
                .map_err(main_slot_error)?;
            Ok(None)
        }
    }

    fn set_session_pane(
        &self,
        workspace: &TmuxWorkspaceHandle,
        qualified_target: &str,
        pane: &TmuxPaneId,
    ) -> Result<(), LifecycleError> {
        let option_name = self.session_pane_option_name(qualified_target);
        self.backend
            .set_session_option(workspace, &option_name, pane.as_str())
            .map_err(main_slot_error)
    }

    fn create_remote_session_pane(
        &self,
        workspace: &TmuxWorkspaceHandle,
        main_pane: &TmuxPaneId,
        current_workspace: &CurrentWorkspace,
        target: &str,
    ) -> Result<TmuxPaneId, LifecycleError> {
        let program = remote_main_slot_program(
            &self.current_executable,
            current_workspace,
            target,
            &self.network,
        );
        self.backend
            .split_pane_bottom_with_program(
                workspace,
                main_pane,
                TmuxSplitSize::Cells(1),
                true,
                &program,
            )
            .map_err(main_slot_error)
    }

    fn configure_main_pane_output_bridge_for_active_target(
        &self,
        workspace: &TmuxWorkspaceHandle,
        _target: Option<&str>,
    ) -> Result<(), LifecycleError> {
        // PaneActivityWatcher handles refresh signaling now; the legacy
        // per-output-line bridge is redundant and causes signal storms.
        self.layout_runtime
            .disable_main_pane_output_bridge(workspace)
    }

    /// One-time migration: find and kill a stale isolation pane (`sleep
    /// infinity`) left over from the old `SessionPaneGuard` architecture,
    /// then swap the main pane back to the display position.
    ///
    /// Only kills panes whose `current_command` contains "sleep" to avoid
    /// touching per-session panes introduced by the new architecture.
    fn cleanup_stale_isolation_pane(
        &self,
        workspace: &TmuxWorkspaceHandle,
        main_pane: &TmuxPaneId,
    ) -> Result<(), LifecycleError> {
        let window = self
            .backend
            .current_window(workspace)
            .map_err(main_slot_error)?;
        let panes = self
            .backend
            .list_panes(workspace, &window)
            .map_err(main_slot_error)?;
        let isolation = panes.iter().find(|p| {
            !p.is_dead
                && p.pane_id != *main_pane
                && p.title != SIDEBAR_PANE_TITLE
                && p.title != FOOTER_PANE_TITLE
                && p.current_command
                    .as_deref()
                    .map_or(false, |cmd| cmd.contains("sleep"))
        });
        if let Some(pane) = isolation {
            let isolation_id = pane.pane_id.clone();
            self.backend
                .kill_pane(workspace, &isolation_id)
                .map_err(main_slot_error)?;
            self.backend
                .swap_panes(workspace, main_pane, &isolation_id)
                .map_err(main_slot_error)?;
        }
        Ok(())
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
        target_registry: &TargetRegistryService<DefaultTargetCatalogGateway>,
        socket_name: &TmuxSocketName,
        target: &str,
    ) -> Result<Option<ManagedSessionRecord>, LifecycleError> {
        target_registry
            .find_target_on_authority(socket_name.as_str(), target)
            .map_err(main_slot_error)
    }

    fn target_registry_for_socket(
        &self,
        socket_name: &str,
    ) -> Result<TargetRegistryService<DefaultTargetCatalogGateway>, LifecycleError> {
        Ok(TargetRegistryService::new(
            DefaultTargetCatalogGateway::from_build_env_with_socket_name(socket_name)
                .map_err(main_slot_error)?,
        ))
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
mod main_slot_runtime_test;
