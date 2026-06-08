use crate::application::layout_service::{FOOTER_PANE_TITLE, SIDEBAR_PANE_TITLE};
use crate::application::target_registry_service::{
    DefaultTargetCatalogGateway, TargetRegistryService,
};
use crate::cli::{prepend_global_network_args, RemoteNetworkConfig};
use crate::cli::{
    ActivateTargetCommand, MainPaneDiedCommand, NewTargetCommand, RemoteTargetExitedCommand,
};
use crate::domain::session_catalog::{ManagedSessionRecord, SessionTransport};
use crate::domain::workspace::WorkspaceSessionRole;
use crate::domain::workspace::{WorkspaceInstanceConfig, WorkspaceInstanceId};
use crate::infra::error_log::ERROR_LOG;
use crate::infra::tmux::{
    EmbeddedTmuxBackend, TmuxError, TmuxGateway, TmuxLayoutGateway, TmuxPaneId, TmuxProgram,
    TmuxSessionGateway, TmuxSessionName, TmuxSocketName, TmuxSplitSize, TmuxWorkspaceHandle,
};
use crate::lifecycle::LifecycleError;
use crate::runtime::remote_node::remote_runtime_owner_runtime::RemoteRuntimeOwnerRuntime;
use crate::runtime::target_host_runtime::TargetHostRuntime;
use crate::runtime::workspace_layout_runtime::WorkspaceLayoutRuntime;
use crate::terminal::TerminalRuntime;
use std::io;
use std::path::Path;
use std::path::PathBuf;

const WAITAGENT_MAIN_PANE_OPTION: &str = "@waitagent_main_pane_id";
const WAITAGENT_ACTIVE_TARGET_OPTION: &str = "@waitagent_active_target";
const WAITAGENT_SESSION_PANE_PREFIX: &str = "@waitagent_session_pane_";
const WAITAGENT_PANE_TARGET_SESSION_OPTION: &str = "@waitagent_target_session_name";
const WAITAGENT_MAIN_PANE_STATE_LOCK_PREFIX: &str = "waitagent-main-pane-state-";
const WAITAGENT_MAIN_PANE_GENERATION_OPTION: &str = "@waitagent_main_pane_generation";
const WAITAGENT_MAIN_PANE_TRANSITION_OPTION: &str = "@waitagent_main_pane_transition";

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
        let _state_guard = self.claim_main_pane_state(&workspace_handle(
            &command.current_socket_name,
            &command.current_session_name,
        ))?;
        let t_activate = std::time::Instant::now();
        ERROR_LOG.log(format!(
            "[diag-timing] run_activate_target: target={}, socket={}, session={}",
            command.target, command.current_socket_name, command.current_session_name
        ));
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
                ERROR_LOG.log(format!(
                    "[diag-timing] run_activate_target: dispatching to remote ({:?})",
                    t_activate.elapsed()
                ));
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
        let workspace =
            workspace_handle(&command.current_socket_name, &command.current_session_name);
        let _state_guard = self.claim_main_pane_state(&workspace)?;
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
        self.activate_target_in_workspace(&current_workspace, &target)
    }

    pub fn run_main_pane_died(&self, command: MainPaneDiedCommand) -> Result<(), LifecycleError> {
        ERROR_LOG.log(format!(
            "[diag-bug] run_main_pane_died: pane={} generation={:?} socket={} session={}",
            command.pane_id, command.pane_generation, command.socket_name, command.session_name
        ));
        let workspace = workspace_handle(&command.socket_name, &command.session_name);
        if !self.workspace_session_is_live(&workspace)? {
            ERROR_LOG.log(format!(
                "[diag-bug] run_main_pane_died ignored stale event for closed workspace: pane={} generation={:?} socket={} session={}",
                command.pane_id, command.pane_generation, command.socket_name, command.session_name
            ));
            return Ok(());
        }
        let _state_guard = match self.claim_main_pane_state(&workspace) {
            Ok(guard) => guard,
            Err(_) if self.workspace_is_closed_for_recovery(&workspace) => {
                ERROR_LOG.log(format!(
                    "[diag-bug] run_main_pane_died ignored event after workspace closed while claiming state: pane={} generation={:?} socket={} session={}",
                    command.pane_id, command.pane_generation, command.socket_name, command.session_name
                ));
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        if !self.workspace_session_is_live(&workspace)? {
            ERROR_LOG.log(format!(
                "[diag-bug] run_main_pane_died ignored event after workspace closed under state lock: pane={} generation={:?} socket={} session={}",
                command.pane_id, command.pane_generation, command.socket_name, command.session_name
            ));
            return Ok(());
        }
        let current_workspace = match self
            .current_workspace_from_names(&command.socket_name, &command.session_name)
        {
            Ok(current_workspace) => current_workspace,
            Err(_) if self.workspace_is_closed_for_recovery(&workspace) => {
                ERROR_LOG.log(format!(
                        "[diag-bug] run_main_pane_died ignored event after workspace closed before metadata read: pane={} generation={:?} socket={} session={}",
                        command.pane_id, command.pane_generation, command.socket_name, command.session_name
                    ));
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        let recovery_pane = TmuxPaneId::new(&command.pane_id);
        let current_main = self
            .backend
            .show_session_option(&workspace, WAITAGENT_MAIN_PANE_OPTION)
            .map_err(main_slot_error)?;
        if current_main.as_deref() != Some(command.pane_id.as_str()) {
            ERROR_LOG.log(format!(
                "[diag-bug] run_main_pane_died ignored stale event: pane={} generation={:?} current_main={current_main:?}",
                command.pane_id, command.pane_generation
            ));
            return Ok(());
        }
        let current_generation = self.main_pane_generation(&workspace)?.unwrap_or_default();
        if let Some(event_generation) = command.pane_generation.as_deref() {
            if current_generation != event_generation {
                ERROR_LOG.log(format!(
                    "[diag-bug] run_main_pane_died ignored stale generation: pane={} event_generation={} current_generation={}",
                    command.pane_id, event_generation, current_generation
                ));
                return Ok(());
            }
        } else {
            ERROR_LOG.log(format!(
                "[diag-bug] run_main_pane_died accepted legacy hook without generation: pane={} current_generation={}",
                command.pane_id, current_generation
            ));
        }
        let active_target = self.active_target(&workspace)?;
        ERROR_LOG.log(format!(
            "[diag-bug] run_main_pane_died: active_target={active_target:?}"
        ));
        let is_remote =
            self.active_target_is_remote(workspace.socket_name.as_str(), active_target.as_deref())?;
        ERROR_LOG.log(format!(
            "[diag-bug] run_main_pane_died: is_remote={is_remote}"
        ));
        if is_remote {
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
            .map_err(|error| {
                ERROR_LOG.log(format!(
                    "[diag-bug] run_main_pane_died: list_targets_on_authority failed: {error:?}"
                ));
                main_slot_error(error)
            })?;
        ERROR_LOG.log(format!(
            "[diag-bug] run_main_pane_died: authority_targets={:?}",
            sessions
                .iter()
                .map(|session| format!(
                    "{} role={:?} availability={:?}",
                    session.address.qualified_target(),
                    session.session_role,
                    session.availability
                ))
                .collect::<Vec<_>>()
        ));
        let next_target =
            next_target_host_session(&sessions, &command.socket_name, active_target.as_deref());
        ERROR_LOG.log(format!(
            "[diag-bug] run_main_pane_died: next_local_target={}",
            next_target
                .as_ref()
                .map_or("none".to_string(), |target| target
                    .address
                    .qualified_target())
        ));

        match next_target {
            Some(target) => {
                ERROR_LOG.log(format!(
                    "[diag-bug] run_main_pane_died: respawning recovery pane {} before activating {}",
                    recovery_pane.as_str(),
                    target.address.qualified_target()
                ));
                self.backend
                    .respawn_pane(
                        &workspace,
                        &recovery_pane,
                        &workspace_shell_program(&current_workspace.workspace_dir),
                    )
                    .map_err(|error| {
                        ERROR_LOG.log(format!(
                            "[diag-bug] run_main_pane_died: respawn recovery pane failed: {error:?}"
                        ));
                        main_slot_error(error)
                    })?;
                ERROR_LOG.log(format!(
                    "[diag-bug] run_main_pane_died: closing active local target identity {active_target:?}"
                ));
                self.close_non_remote_target_session_identity(active_target.as_deref())
                    .map_err(|error| {
                        ERROR_LOG.log(format!(
                            "[diag-bug] run_main_pane_died: close active local target failed: {error}"
                        ));
                        error
                    })?;
                self.set_active_target(&workspace, None).map_err(|error| {
                    ERROR_LOG.log(format!(
                        "[diag-bug] run_main_pane_died: clear active target failed: {error}"
                    ));
                    error
                })?;
                self.activate_target_in_workspace(&current_workspace, &target)
                    .map_err(|error| {
                        ERROR_LOG.log(format!(
                            "[diag-bug] run_main_pane_died: activate fallback target failed: {error}"
                        ));
                        error
                    })
            }
            None => {
                ERROR_LOG.log(format!(
                    "[diag-bug] run_main_pane_died: no local fallback, closing active target {active_target:?} and killing socket"
                ));
                self.close_non_remote_target_session_identity(active_target.as_deref())
                    .map_err(|error| {
                        ERROR_LOG.log(format!(
                            "[diag-bug] run_main_pane_died: close final active target failed: {error}"
                        ));
                        error
                    })?;
                self.backend
                    .kill_server(&TmuxSocketName::new(workspace.socket_name.as_str()))
                    .map_err(|error| {
                        ERROR_LOG.log(format!(
                            "[diag-bug] run_main_pane_died: kill socket failed: {error:?}"
                        ));
                        main_slot_error(error)
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
        let active_target = self.active_target(&workspace)?;
        let switching_from_remote =
            self.active_target_is_remote(workspace.socket_name.as_str(), active_target.as_deref())?;
        let previous_remote_main_pane =
            self.active_remote_session_pane(&workspace, active_target.as_deref())?;
        if switching_from_remote && !self.pane_is_live(&workspace, workspace_main_pane.as_str()) {
            workspace_main_pane = previous_remote_main_pane
                .as_ref()
                .filter(|pane| self.pane_is_live(&workspace, pane.as_str()))
                .cloned()
                .or_else(|| self.infer_target_main_pane(&workspace).map(TmuxPaneId::new))
                .ok_or_else(|| {
                    LifecycleError::Protocol(format!(
                        "workspace `{}` has no live recovery pane after remote exit",
                        workspace.session_name.as_str()
                    ))
                })?;
        }
        self.begin_main_pane_transition(&workspace, &workspace_main_pane)?;

        if let Some(active_target) = active_target.as_deref() {
            if let Some((_, target_session)) = split_qualified_target(active_target)
                .filter(|(sock, _)| *sock == workspace.socket_name.as_str())
            {
                let target_workspace = self.target_workspace_on_socket(&workspace, target_session);
                let parking_pane =
                    self.local_target_parking_pane(&target_workspace, active_target)?;
                if parking_pane != workspace_main_pane {
                    self.backend
                        .swap_panes(&workspace, &parking_pane, &workspace_main_pane)
                        .map_err(main_slot_error)?;
                    self.set_local_target_pane_identity(
                        &workspace,
                        &workspace_main_pane,
                        target_session,
                    )?;
                    self.set_workspace_main_pane(&target_workspace, &workspace_main_pane)?;
                    workspace_main_pane = parking_pane;
                    self.clear_local_target_pane_identity(&workspace, &workspace_main_pane)?;
                }
            } else if switching_from_remote {
                workspace_main_pane = self.prepare_remote_main_pane_for_local_switch(
                    current_workspace,
                    &workspace,
                    &workspace_main_pane,
                    previous_remote_main_pane.as_ref(),
                )?;
                self.cleanup_stale_isolation_pane(&workspace, &workspace_main_pane)?;
            }
        }

        let target_main_pane = self.target_main_pane(target)?;
        if target_main_pane != workspace_main_pane {
            self.backend
                .swap_panes(&workspace, &target_main_pane, &workspace_main_pane)
                .map_err(main_slot_error)?;
            self.clear_local_target_pane_identity(&workspace, &workspace_main_pane)?;
        }
        self.set_local_target_pane_identity(
            &workspace,
            &target_main_pane,
            target.address.session_id(),
        )?;
        let target_workspace =
            self.target_workspace_on_socket(&workspace, target.address.session_id());
        self.set_workspace_main_pane(&target_workspace, &target_main_pane)?;

        let _ = self.backend.select_pane(&workspace, &target_main_pane);
        self.restore_workspace_main_pane(
            current_workspace,
            &workspace,
            &target_main_pane,
            Some(target.address.qualified_target().as_str()),
        )?;
        if let Some(previous_remote_main_pane) = previous_remote_main_pane.as_ref() {
            self.deactivate_inactive_remote_session_pane(&workspace, previous_remote_main_pane);
            self.clear_stale_remote_session_pane(
                &workspace,
                active_target.as_deref(),
                previous_remote_main_pane,
            )?;
        }
        Ok(())
    }

    fn activate_remote_target_in_workspace(
        &self,
        current_workspace: &CurrentWorkspace,
        target: &crate::domain::session_catalog::ManagedSessionRecord,
    ) -> Result<(), LifecycleError> {
        let t_start = std::time::Instant::now();
        let target_id = target.address.id().as_str().to_string();
        let qualified_target = target.address.qualified_target();
        ERROR_LOG.log(format!(
            "[diag-timing][{}] activate_remote_target_in_workspace: start",
            target_id
        ));

        let workspace = workspace_handle(
            &current_workspace.socket_name,
            &current_workspace.session_name,
        );
        if self.active_target(&workspace)?.as_deref() == Some(qualified_target.as_str()) {
            ERROR_LOG.log(format!(
                "[diag-timing][{}] already active target, re-syncing bindings ({:?})",
                target_id,
                t_start.elapsed()
            ));
            self.layout_runtime
                .disable_main_pane_output_bridge(&workspace)?;
            self.layout_runtime
                .sync_main_slot_bindings(&workspace, &current_workspace.workspace_dir)?;
            return Ok(());
        }

        let mut workspace_main_pane = self.workspace_main_pane(&workspace)?;
        let active_target = self.active_target(&workspace)?;
        let previous_remote_main_pane =
            self.active_remote_session_pane(&workspace, active_target.as_deref())?;
        self.begin_main_pane_transition(&workspace, &workspace_main_pane)?;
        ERROR_LOG.log(format!(
            "[diag-timing][{}] got workspace_main_pane={:?} ({:?})",
            target_id,
            workspace_main_pane,
            t_start.elapsed()
        ));

        // If switching from a local target host on the same socket, swap its
        // pane back to its own position before swapping in the remote session.
        if let Some(active_target) = active_target.as_deref() {
            ERROR_LOG.log(format!(
                "[diag][{}] active_target={:?}",
                target_id, active_target
            ));
            if let Some((_, target_session)) = split_qualified_target(active_target)
                .filter(|(sock, _)| *sock == workspace.socket_name.as_str())
            {
                let target_workspace = self.target_workspace_on_socket(&workspace, target_session);
                ERROR_LOG.log(format!(
                    "[diag][{}] swapping local host pane back for target_workspace={:?}",
                    target_id, target_workspace.session_name
                ));
                let parking_pane =
                    self.local_target_parking_pane(&target_workspace, active_target)?;
                ERROR_LOG.log(format!(
                    "[diag][{}] swapping parking pane {:?} with workspace_main_pane {:?}",
                    target_id, parking_pane, workspace_main_pane
                ));
                if parking_pane != workspace_main_pane {
                    self.backend
                        .swap_panes(&workspace, &parking_pane, &workspace_main_pane)
                        .map_err(main_slot_error)?;
                    self.set_local_target_pane_identity(
                        &workspace,
                        &workspace_main_pane,
                        target_session,
                    )?;
                    self.set_workspace_main_pane(&target_workspace, &workspace_main_pane)?;
                    workspace_main_pane = parking_pane;
                    self.clear_local_target_pane_identity(&workspace, &workspace_main_pane)?;
                }
            }
        }

        // One-time migration: clean up stale isolation panes from old architecture
        let t_before_cleanup = std::time::Instant::now();
        self.cleanup_stale_isolation_pane(&workspace, &workspace_main_pane)?;
        ERROR_LOG.log(format!(
            "[diag-timing][{}] after cleanup_stale_isolation_pane, workspace_main_pane={:?} (cleanup={:?}, total={:?})",
            target_id, workspace_main_pane, t_before_cleanup.elapsed(), t_start.elapsed()
        ));

        // Find or create a persistent per-session pane for this remote target
        let t_before_find = std::time::Instant::now();
        let session_pane = match self.find_session_pane(&workspace, &qualified_target)? {
            Some(existing_pane) => {
                ERROR_LOG.log(format!(
                    "[diag-timing][{}] found existing session_pane={:?} ({:?})",
                    target_id,
                    existing_pane,
                    t_start.elapsed()
                ));
                existing_pane
            }
            None => {
                ERROR_LOG.log(format!(
                    "[diag-timing][{}] creating new remote session pane (find took {:?}, total {:?})",
                    target_id, t_before_find.elapsed(), t_start.elapsed()
                ));
                let t_before_create = std::time::Instant::now();
                let new_pane = self.create_remote_session_pane(
                    &workspace,
                    &workspace_main_pane,
                    current_workspace,
                    target,
                )?;
                ERROR_LOG.log(format!(
                    "[diag-timing][{}] new remote session pane={:?} (create={:?}, total={:?})",
                    target_id,
                    new_pane,
                    t_before_create.elapsed(),
                    t_start.elapsed()
                ));
                self.set_session_pane(&workspace, &qualified_target, &new_pane)?;
                new_pane
            }
        };

        // Swap the session pane into the display position
        ERROR_LOG.log(format!(
            "[diag][{}] session_pane={:?}, workspace_main_pane={:?}",
            target_id, session_pane, workspace_main_pane
        ));

        let t_before_swap = std::time::Instant::now();
        if session_pane != workspace_main_pane {
            self.backend
                .swap_panes(&workspace, &session_pane, &workspace_main_pane)
                .map_err(|e| {
                    ERROR_LOG.log(format!(
                        "[diag-timing][{}] swap_panes FAILED: {:?} ({:?})",
                        target_id,
                        e,
                        t_start.elapsed()
                    ));
                    main_slot_error(e)
                })?;
            ERROR_LOG.log(format!(
                "[diag-timing][{}] swap_panes done ({:?})",
                target_id,
                t_before_swap.elapsed()
            ));

            // Move the leftover 1-cell pane to a detached helper window so
            // the process stays alive but the workspace layout stays clean.
            // After swap_panes, workspace_main_pane holds the old content at
            // the 1-cell position where split_pane_bottom created the pane.
            let _ = self.backend.run_on_socket(
                &workspace.socket_name,
                &[
                    "break-pane".to_string(),
                    "-d".to_string(),
                    "-s".to_string(),
                    workspace_main_pane.as_str().to_string(),
                    "-n".to_string(),
                    format!(
                        "wa-orphan-{}",
                        workspace_main_pane.as_str().trim_start_matches('%')
                    ),
                ],
            );
            // split_pane_bottom + swap_panes + break-pane redistributes
            // the window space and can give the footer an extra line.
            // Reset it back to 1 cell before the chrome refresh fires.
            if let Ok(window) = self.backend.current_window(&workspace) {
                if let Ok(panes) = self.backend.list_panes(&workspace, &window) {
                    if let Some(footer) = panes
                        .iter()
                        .find(|p| p.title == FOOTER_PANE_TITLE && !p.is_dead)
                    {
                        let _ = self.backend.set_pane_height(&workspace, &footer.pane_id, 1);
                    }
                }
            }
        } else {
            ERROR_LOG.log(format!(
                "[diag-timing][{}] session pane already at display position ({:?})",
                target_id,
                t_start.elapsed()
            ));
        }

        // Select the session pane so keyboard focus follows the swap.
        // Without this, keystrokes may still land on the previous pane.
        // Must happen unconditionally: the session_pane != workspace_main_pane
        // check skips the swap when the remote pane is already at the display
        // position (e.g. on second activation), but keyboard focus was
        // never set there either.
        let _ = self.backend.select_pane(&workspace, &session_pane);

        ERROR_LOG.log(format!(
            "[diag-timing][{}] set_workspace_main_pane + set_active_target done ({:?})",
            target_id,
            t_start.elapsed()
        ));
        let result = self.restore_workspace_main_pane(
            current_workspace,
            &workspace,
            &session_pane,
            Some(qualified_target.as_str()),
        );
        if result.is_ok() {
            if let Some(previous_remote_main_pane) = previous_remote_main_pane.as_ref() {
                if previous_remote_main_pane != &session_pane {
                    self.deactivate_inactive_remote_session_pane(
                        &workspace,
                        previous_remote_main_pane,
                    );
                    self.clear_stale_remote_session_pane(
                        &workspace,
                        active_target.as_deref(),
                        previous_remote_main_pane,
                    )?;
                }
            }
        }
        ERROR_LOG.log(format!(
            "[diag-timing][{}] refresh_workspace_chrome done result={:?} (total={:?})",
            target_id,
            result,
            t_start.elapsed()
        ));
        result
    }

    fn fallback_after_remote_main_pane_exit(
        &self,
        current_workspace: &CurrentWorkspace,
        workspace: &TmuxWorkspaceHandle,
        recovery_pane: &TmuxPaneId,
        active_target: Option<String>,
    ) -> Result<(), LifecycleError> {
        let sessions = self.workspace_visible_targets(
            workspace.socket_name.as_str(),
            workspace.session_name.as_str(),
            active_target.as_deref(),
        )?;
        if let Some(active_remote_target) = active_target
            .as_deref()
            .and_then(|target_id| {
                sessions.iter().find(|session| {
                    session.address.qualified_target() == target_id
                        && (session.address.transport() == &SessionTransport::RemotePeer
                            || session.address.authority_id().contains('#'))
                })
            })
            .cloned()
        {
            return self.recover_remote_target_in_workspace(
                current_workspace,
                workspace,
                recovery_pane,
                &active_remote_target,
            );
        }
        ERROR_LOG.log(format!(
            "[diag-bug] fallback: found {} visible workspace sessions, active_target={active_target:?}",
            sessions.len()
        ));
        let next_target =
            next_remote_fallback_target(&sessions, active_target.as_deref()).or_else(|| {
                next_target_host_session(
                    &sessions,
                    workspace.socket_name.as_str(),
                    active_target.as_deref(),
                )
            });
        ERROR_LOG.log(format!(
            "[diag-bug] fallback: next_target={}",
            next_target.as_ref().map_or("none".to_string(), |t| t
                .address
                .id()
                .as_str()
                .to_string())
        ));
        self.close_non_remote_target_session_identity(active_target.as_deref())?;
        match next_target {
            Some(target) => {
                // Sessions published from a remote node may appear as
                // local-tmux or remote-peer depending on the publication
                // path (session-sync vs ingress). Treat as remote whenever
                // the authority id contains a port separator `#`, which
                // means it came from another node.
                let is_remote = target.address.transport() == &SessionTransport::RemotePeer
                    || target.address.authority_id().contains('#');
                ERROR_LOG.log(format!(
                    "[diag-bug] fallback: activating target={} transport={:?} authority={} is_remote={is_remote}",
                    target.address.id().as_str(),
                    target.address.transport(),
                    target.address.authority_id(),
                ));
                if is_remote {
                    self.activate_remote_target_in_workspace(current_workspace, &target)?;
                } else {
                    let recovery_pane = self.resolve_recovery_pane(workspace, recovery_pane)?;
                    self.backend
                        .respawn_pane(
                            workspace,
                            &recovery_pane,
                            &workspace_shell_program(&current_workspace.workspace_dir),
                        )
                        .map_err(main_slot_error)?;
                    self.clear_remote_recovery_pane_state(workspace, &recovery_pane);
                    self.activate_target_in_workspace(current_workspace, &target)?;
                }
            }
            None => {
                ERROR_LOG.log(
                    "[diag-bug] fallback: no next target, respawning with host only".to_string(),
                );
                let recovery_pane = self.resolve_recovery_pane(workspace, recovery_pane)?;
                self.backend
                    .respawn_pane(
                        workspace,
                        &recovery_pane,
                        &workspace_shell_program(&current_workspace.workspace_dir),
                    )
                    .map_err(main_slot_error)?;
                self.clear_remote_recovery_pane_state(workspace, &recovery_pane);
                self.restore_workspace_main_pane(
                    current_workspace,
                    workspace,
                    &recovery_pane,
                    None,
                )?;
            }
        }
        Ok(())
    }

    pub fn run_remote_target_exited(
        &self,
        command: RemoteTargetExitedCommand,
    ) -> Result<(), LifecycleError> {
        ERROR_LOG.log(format!(
            "[diag-native] run_remote_target_exited: target={} socket={} session={} pane={:?}",
            command.target, command.socket_name, command.session_name, command.pane_id
        ));
        let current_workspace =
            self.current_workspace_from_names(&command.socket_name, &command.session_name)?;
        let workspace = workspace_handle(&command.socket_name, &command.session_name);
        let _state_guard = self.claim_main_pane_state(&workspace)?;
        let active_target = self.active_target(&workspace)?;
        if active_target.as_deref() != Some(command.target.as_str()) {
            ERROR_LOG.log(format!(
                "[diag-native] run_remote_target_exited ignored stale event: target={} active_target={active_target:?}",
                command.target
            ));
            return Ok(());
        }

        self.remove_remote_target_runtime_record(&command.socket_name, &command.target)?;
        let session_pane = self.find_session_pane(&workspace, &command.target)?;
        let fallback =
            self.next_target_after_remote_exit(&workspace, Some(command.target.as_str()))?;
        let mut cleanup_pane = session_pane.clone();

        match fallback {
            Some(target) => {
                let is_remote = target.address.transport() == &SessionTransport::RemotePeer
                    || target.address.authority_id().contains('#');
                if is_remote {
                    let recovery_pane = match cleanup_pane.as_ref().cloned() {
                        Some(pane) => pane,
                        None => self.workspace_main_pane(&workspace)?,
                    };
                    self.recover_remote_target_in_workspace(
                        &current_workspace,
                        &workspace,
                        &recovery_pane,
                        &target,
                    )?;
                    cleanup_pane = None;
                } else {
                    self.activate_target_in_workspace(&current_workspace, &target)?;
                }
            }
            None => {
                self.cleanup_exited_remote_session_pane(
                    &workspace,
                    &command.target,
                    cleanup_pane.as_ref(),
                )?;
                self.backend
                    .kill_server(&TmuxSocketName::new(workspace.socket_name.as_str()))
                    .map_err(main_slot_error)?;
                return Ok(());
            }
        }

        self.cleanup_exited_remote_session_pane(
            &workspace,
            &command.target,
            cleanup_pane.as_ref(),
        )?;
        Ok(())
    }

    fn clear_remote_recovery_pane_state(
        &self,
        workspace: &TmuxWorkspaceHandle,
        recovery_pane: &TmuxPaneId,
    ) {
        let _ = self
            .backend
            .unset_pane_hook(workspace, recovery_pane, "pane-died");
        let _ = self
            .backend
            .set_pane_option(workspace, recovery_pane, "remain-on-exit", "off");
    }

    fn prepare_remote_main_pane_for_local_switch(
        &self,
        current_workspace: &CurrentWorkspace,
        workspace: &TmuxWorkspaceHandle,
        workspace_main_pane: &TmuxPaneId,
        previous_remote_main_pane: Option<&TmuxPaneId>,
    ) -> Result<TmuxPaneId, LifecycleError> {
        if previous_remote_main_pane != Some(workspace_main_pane) {
            return Ok(workspace_main_pane.clone());
        }

        let (parking_pane, break_remote_after_swap) =
            match self.find_workspace_orphan_parking_pane(workspace, workspace_main_pane)? {
                Some(pane) => (pane, false),
                None => (
                    self.create_workspace_parking_pane(
                        current_workspace,
                        workspace,
                        workspace_main_pane,
                    )?,
                    true,
                ),
            };

        if parking_pane == *workspace_main_pane {
            return Ok(workspace_main_pane.clone());
        }

        ERROR_LOG.log(format!(
            "[diag-native] remote-to-local parking swap: remote_pane={} parking_pane={} break_remote_after_swap={}",
            workspace_main_pane.as_str(),
            parking_pane.as_str(),
            break_remote_after_swap
        ));
        self.backend
            .swap_panes(workspace, &parking_pane, workspace_main_pane)
            .map_err(main_slot_error)?;
        if break_remote_after_swap {
            self.break_pane_to_orphan_window(workspace, workspace_main_pane);
        }
        Ok(parking_pane)
    }

    fn find_workspace_orphan_parking_pane(
        &self,
        workspace: &TmuxWorkspaceHandle,
        excluded_pane: &TmuxPaneId,
    ) -> Result<Option<TmuxPaneId>, LifecycleError> {
        let output = self
            .backend
            .run_on_socket(
                &workspace.socket_name,
                &[
                    "list-panes".to_string(),
                    "-a".to_string(),
                    "-F".to_string(),
                    format!(
                        "#{{session_name}}\t#{{window_name}}\t#{{pane_id}}\t#{{pane_dead}}\t#{{pane_title}}\t#{{pane_current_command}}\t#{{{WAITAGENT_PANE_TARGET_SESSION_OPTION}}}"
                    ),
                ],
            )
            .map_err(main_slot_error)?;
        for line in output.stdout.lines() {
            let mut parts = line.split('\t');
            let session_name = parts.next().unwrap_or_default();
            let window_name = parts.next().unwrap_or_default();
            let pane_id = parts.next().unwrap_or_default();
            let pane_dead = parts.next().unwrap_or_default();
            let title = parts.next().unwrap_or_default();
            let current_command = parts.next().unwrap_or_default();
            let identity = parts.next().unwrap_or_default();
            if session_name == workspace.session_name.as_str()
                && window_name.starts_with("wa-orphan-")
                && pane_dead == "0"
                && !pane_id.is_empty()
                && pane_id != excluded_pane.as_str()
                && title != SIDEBAR_PANE_TITLE
                && title != FOOTER_PANE_TITLE
                && current_command != "waitagent"
                && identity.is_empty()
            {
                return Ok(Some(TmuxPaneId::new(pane_id)));
            }
        }
        Ok(None)
    }

    fn create_workspace_parking_pane(
        &self,
        current_workspace: &CurrentWorkspace,
        workspace: &TmuxWorkspaceHandle,
        main_pane: &TmuxPaneId,
    ) -> Result<TmuxPaneId, LifecycleError> {
        self.backend
            .split_pane_bottom_with_program(
                workspace,
                main_pane,
                TmuxSplitSize::Cells(1),
                true,
                &workspace_shell_program(&current_workspace.workspace_dir),
            )
            .map_err(main_slot_error)
    }

    fn break_pane_to_orphan_window(&self, workspace: &TmuxWorkspaceHandle, pane: &TmuxPaneId) {
        let _ = self.backend.run_on_socket(
            &workspace.socket_name,
            &[
                "break-pane".to_string(),
                "-d".to_string(),
                "-s".to_string(),
                pane.as_str().to_string(),
                "-n".to_string(),
                format!("wa-orphan-{}", pane.as_str().trim_start_matches('%')),
            ],
        );
    }

    fn deactivate_inactive_remote_session_pane(
        &self,
        workspace: &TmuxWorkspaceHandle,
        pane: &TmuxPaneId,
    ) {
        let _ = self.backend.unset_pane_hook(workspace, pane, "pane-died");
        let _ = self
            .backend
            .set_pane_option(workspace, pane, "remain-on-exit", "off");
    }

    fn clear_stale_remote_session_pane(
        &self,
        workspace: &TmuxWorkspaceHandle,
        qualified_target: Option<&str>,
        pane: &TmuxPaneId,
    ) -> Result<(), LifecycleError> {
        let Some(qualified_target) = qualified_target else {
            return Ok(());
        };
        if !self.active_target_is_remote(workspace.socket_name.as_str(), Some(qualified_target))? {
            return Ok(());
        }
        if self.pane_is_live(workspace, pane.as_str()) {
            return Ok(());
        }
        self.clear_session_pane(workspace, qualified_target)
    }

    fn restore_workspace_main_pane(
        &self,
        current_workspace: &CurrentWorkspace,
        workspace: &TmuxWorkspaceHandle,
        pane: &TmuxPaneId,
        active_target: Option<&str>,
    ) -> Result<(), LifecycleError> {
        let previous_main_pane = self
            .backend
            .show_session_option(workspace, WAITAGENT_MAIN_PANE_OPTION)
            .map_err(main_slot_error)?
            .filter(|previous| previous != pane.as_str())
            .map(TmuxPaneId::new);
        if let Some(previous_main_pane) = previous_main_pane.as_ref() {
            if self.pane_exists(workspace, previous_main_pane.as_str()) {
                let _ = self
                    .backend
                    .unset_pane_hook(workspace, previous_main_pane, "pane-died");
                let _ = self.backend.set_pane_option(
                    workspace,
                    previous_main_pane,
                    "remain-on-exit",
                    "off",
                );
            }
        }
        let _ = self.backend.select_pane(workspace, pane);
        self.set_workspace_main_pane(workspace, pane)?;
        self.set_active_target(workspace, active_target)?;
        self.backend
            .set_session_option(workspace, WAITAGENT_MAIN_PANE_TRANSITION_OPTION, "")
            .map_err(main_slot_error)?;
        self.layout_runtime
            .disable_main_pane_output_bridge(workspace)?;
        self.layout_runtime
            .sync_main_slot_bindings(workspace, &current_workspace.workspace_dir)?;
        let result = self
            .layout_runtime
            .refresh_workspace_chrome(workspace, &current_workspace.workspace_dir);
        let effective_pane = self.workspace_main_pane(workspace)?;
        if effective_pane != *pane {
            ERROR_LOG.log(format!(
                "[diag-bug] restore_workspace_main_pane remapped pane from {} to {} after refresh",
                pane.as_str(),
                effective_pane.as_str()
            ));
        }
        let pane_generation = self.bump_main_pane_generation(workspace)?;
        let pane_died_command = self
            .layout_runtime
            .main_pane_died_hook_command(workspace, &pane_generation);
        self.backend
            .set_pane_hook(workspace, &effective_pane, "pane-died", &pane_died_command)
            .map_err(main_slot_error)?;
        self.backend
            .set_pane_option(workspace, &effective_pane, "remain-on-exit", "on")
            .map_err(main_slot_error)?;
        result
    }

    fn recover_remote_target_in_workspace(
        &self,
        current_workspace: &CurrentWorkspace,
        workspace: &TmuxWorkspaceHandle,
        recovery_pane: &TmuxPaneId,
        target: &ManagedSessionRecord,
    ) -> Result<(), LifecycleError> {
        if !self.pane_exists(workspace, recovery_pane.as_str()) {
            self.clear_session_pane(workspace, target.address.qualified_target().as_str())?;
            return self.activate_remote_target_in_workspace(current_workspace, target);
        }

        self.clear_remote_recovery_pane_state(workspace, recovery_pane);
        self.backend
            .respawn_pane(
                workspace,
                recovery_pane,
                &remote_main_slot_program(
                    &self.current_executable,
                    current_workspace,
                    target,
                    &self.network,
                ),
            )
            .map_err(main_slot_error)?;
        self.set_session_pane(
            workspace,
            target.address.qualified_target().as_str(),
            recovery_pane,
        )?;
        self.restore_workspace_main_pane(
            current_workspace,
            workspace,
            recovery_pane,
            Some(target.address.qualified_target().as_str()),
        )
    }

    fn resolve_recovery_pane(
        &self,
        workspace: &TmuxWorkspaceHandle,
        recovery_pane: &TmuxPaneId,
    ) -> Result<TmuxPaneId, LifecycleError> {
        if self.pane_exists(workspace, recovery_pane.as_str()) {
            return Ok(recovery_pane.clone());
        }
        self.workspace_main_pane(workspace)
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
        self.local_target_content_pane(&workspace, session.address.qualified_target().as_str())
    }

    fn local_target_content_pane(
        &self,
        workspace: &TmuxWorkspaceHandle,
        qualified_target: &str,
    ) -> Result<TmuxPaneId, LifecycleError> {
        let target_session_name = workspace.session_name.as_str();
        if let Some(pane) =
            self.find_local_target_pane_by_identity(workspace, target_session_name)?
        {
            self.set_workspace_main_pane(workspace, &pane)?;
            return Ok(pane);
        }
        if let Some(pane) = self.read_live_session_pane_option_for_target(
            workspace,
            WAITAGENT_MAIN_PANE_OPTION,
            target_session_name,
        )? {
            self.set_local_target_pane_identity(workspace, &pane, target_session_name)?;
            return Ok(pane);
        }
        let pane = self.local_target_parking_pane(workspace, qualified_target)?;
        self.set_local_target_pane_identity(workspace, &pane, target_session_name)?;
        self.set_workspace_main_pane(workspace, &pane)?;
        Ok(pane)
    }

    fn local_target_parking_pane(
        &self,
        workspace: &TmuxWorkspaceHandle,
        qualified_target: &str,
    ) -> Result<TmuxPaneId, LifecycleError> {
        self.backend
            .target_main_pane_on_socket(
                workspace.socket_name.as_str(),
                workspace.session_name.as_str(),
            )
            .map_err(main_slot_error)
            .map_err(|error| {
                LifecycleError::Protocol(format!(
                    "target `{}` has no available parking pane: {error}",
                    qualified_target
                ))
            })
    }

    fn set_local_target_pane_identity(
        &self,
        workspace: &TmuxWorkspaceHandle,
        pane: &TmuxPaneId,
        target_session_name: &str,
    ) -> Result<(), LifecycleError> {
        self.backend
            .set_pane_option(
                workspace,
                pane,
                WAITAGENT_PANE_TARGET_SESSION_OPTION,
                target_session_name,
            )
            .map_err(main_slot_error)
    }

    fn clear_local_target_pane_identity(
        &self,
        workspace: &TmuxWorkspaceHandle,
        pane: &TmuxPaneId,
    ) -> Result<(), LifecycleError> {
        if !self.pane_exists_on_socket(workspace, pane.as_str())? {
            return Ok(());
        }
        self.backend
            .set_pane_option(workspace, pane, WAITAGENT_PANE_TARGET_SESSION_OPTION, "")
            .map_err(main_slot_error)
    }

    fn read_live_session_pane_option_for_target(
        &self,
        workspace: &TmuxWorkspaceHandle,
        option_name: &str,
        target_session_name: &str,
    ) -> Result<Option<TmuxPaneId>, LifecycleError> {
        let pane_id = self
            .backend
            .show_session_option(workspace, option_name)
            .map_err(main_slot_error)?
            .filter(|pane| !pane.is_empty());
        let Some(pane_id) = pane_id else {
            return Ok(None);
        };
        let pane = TmuxPaneId::new(pane_id);
        if self.pane_is_live(workspace, pane.as_str())
            && self.pane_target_identity(workspace, &pane)?.as_deref() == Some(target_session_name)
        {
            return Ok(Some(pane));
        }
        self.backend
            .set_session_option(workspace, option_name, "")
            .map_err(main_slot_error)?;
        Ok(None)
    }

    fn find_local_target_pane_by_identity(
        &self,
        workspace: &TmuxWorkspaceHandle,
        target_session_name: &str,
    ) -> Result<Option<TmuxPaneId>, LifecycleError> {
        let output = self
            .backend
            .run_on_socket(
                &workspace.socket_name,
                &[
                    "list-panes".to_string(),
                    "-a".to_string(),
                    "-F".to_string(),
                    format!(
                        "#{{session_name}}\t#{{pane_id}}\t#{{pane_dead}}\t#{{pane_title}}\t#{{{WAITAGENT_PANE_TARGET_SESSION_OPTION}}}"
                    ),
                ],
            )
            .map_err(main_slot_error)?;
        let mut fallback = None;
        for line in output.stdout.lines() {
            let mut parts = line.split('\t');
            let session_name = parts.next().unwrap_or_default();
            let pane_id = parts.next().unwrap_or_default();
            let pane_dead = parts.next().unwrap_or_default();
            let title = parts.next().unwrap_or_default();
            let identity = parts.next().unwrap_or_default();
            if pane_dead != "0"
                || pane_id.is_empty()
                || title == SIDEBAR_PANE_TITLE
                || title == FOOTER_PANE_TITLE
                || identity != target_session_name
            {
                continue;
            }
            let pane = TmuxPaneId::new(pane_id);
            if session_name == target_session_name {
                return Ok(Some(pane));
            }
            fallback.get_or_insert(pane);
        }
        Ok(fallback)
    }

    fn pane_target_identity(
        &self,
        workspace: &TmuxWorkspaceHandle,
        pane: &TmuxPaneId,
    ) -> Result<Option<String>, LifecycleError> {
        let output = self
            .backend
            .run_on_socket(
                &workspace.socket_name,
                &[
                    "show-options".to_string(),
                    "-pqv".to_string(),
                    "-t".to_string(),
                    pane.as_str().to_string(),
                    WAITAGENT_PANE_TARGET_SESSION_OPTION.to_string(),
                ],
            )
            .map_err(main_slot_error)?;
        let value = output.stdout.trim();
        Ok((!value.is_empty()).then(|| value.to_string()))
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

    fn remove_remote_target_runtime_record(
        &self,
        socket_name: &str,
        qualified_target: &str,
    ) -> Result<(), LifecycleError> {
        let Some((authority_id, transport_session_id)) = split_qualified_target(qualified_target)
        else {
            return Ok(());
        };
        RemoteRuntimeOwnerRuntime::from_build_env_with_network(self.network.clone())?
            .remove_session(
                socket_name,
                authority_id,
                authority_id,
                transport_session_id,
            )
    }

    fn close_non_remote_target_session_identity(
        &self,
        target: Option<&str>,
    ) -> Result<(), LifecycleError> {
        let Some(target) = target else {
            return Ok(());
        };
        if self
            .target_registry
            .find_target(target)
            .map_err(main_slot_error)?
            .is_some_and(|session| session.address.transport() == &SessionTransport::RemotePeer)
        {
            return Ok(());
        }
        if target.contains('#') {
            return Ok(());
        }
        self.close_target_session_identity(Some(target))
    }

    fn next_target_after_remote_exit(
        &self,
        workspace: &TmuxWorkspaceHandle,
        active_target: Option<&str>,
    ) -> Result<Option<ManagedSessionRecord>, LifecycleError> {
        let sessions = self.workspace_visible_targets(
            workspace.socket_name.as_str(),
            workspace.session_name.as_str(),
            active_target,
        )?;
        Ok(next_remote_fallback_target(&sessions, active_target))
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

    fn workspace_session_is_live(
        &self,
        workspace: &TmuxWorkspaceHandle,
    ) -> Result<bool, LifecycleError> {
        match self.backend.run_on_socket(
            &workspace.socket_name,
            &[
                "has-session".to_string(),
                "-t".to_string(),
                workspace.session_name.as_str().to_string(),
            ],
        ) {
            Ok(_) => Ok(true),
            Err(error) if error.is_command_failure() => Ok(false),
            Err(error) => Err(main_slot_error(error)),
        }
    }

    fn workspace_is_closed_for_recovery(&self, workspace: &TmuxWorkspaceHandle) -> bool {
        matches!(self.workspace_session_is_live(workspace), Ok(false))
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
        // Remote authority targets carry an IP:port#port prefix.  Short-
        // circuit without querying the catalog, which may not have the
        // remote session yet when called from a short-lived __main-pane-died
        // subprocess.
        if target.contains('#') {
            return Ok(true);
        }
        Ok(self.remote_target_record(socket_name, target)?.is_some())
    }

    fn begin_main_pane_transition(
        &self,
        workspace: &TmuxWorkspaceHandle,
        _pane: &TmuxPaneId,
    ) -> Result<(), LifecycleError> {
        // Keep the old pane's remain-on-exit protection until the new main
        // pane is restored. Remote panes can exit while a switch is in
        // progress; dropping remain-on-exit here lets tmux delete the pane
        // before swap-pane has a stable target.
        self.backend
            .set_session_option(workspace, WAITAGENT_MAIN_PANE_TRANSITION_OPTION, "1")
            .map_err(main_slot_error)?;
        self.backend
            .set_session_option(workspace, WAITAGENT_MAIN_PANE_OPTION, "")
            .map_err(main_slot_error)?;
        self.bump_main_pane_generation(workspace).map(|_| ())
    }

    fn claim_main_pane_state<'a>(
        &'a self,
        workspace: &TmuxWorkspaceHandle,
    ) -> Result<MainPaneStateGuard<'a>, LifecycleError> {
        let lock_name = format!(
            "{WAITAGENT_MAIN_PANE_STATE_LOCK_PREFIX}{}",
            workspace.session_name.as_str()
        );
        self.backend
            .run_on_socket(
                &TmuxSocketName::new(workspace.socket_name.as_str()),
                &["wait-for".to_string(), "-L".to_string(), lock_name.clone()],
            )
            .map_err(main_slot_error)?;
        Ok(MainPaneStateGuard {
            backend: &self.backend,
            socket_name: TmuxSocketName::new(workspace.socket_name.as_str()),
            lock_name,
        })
    }

    fn target_workspace_on_socket(
        &self,
        workspace: &TmuxWorkspaceHandle,
        target_session: &str,
    ) -> TmuxWorkspaceHandle {
        TmuxWorkspaceHandle {
            workspace_id: WorkspaceInstanceId::new(target_session),
            socket_name: TmuxSocketName::new(workspace.socket_name.as_str()),
            session_name: TmuxSessionName::new(target_session),
        }
    }

    fn active_remote_session_pane(
        &self,
        workspace: &TmuxWorkspaceHandle,
        active_target: Option<&str>,
    ) -> Result<Option<TmuxPaneId>, LifecycleError> {
        let Some(active_target) = active_target else {
            return Ok(None);
        };
        if !self.active_target_is_remote(workspace.socket_name.as_str(), Some(active_target))? {
            return Ok(None);
        }
        self.find_session_pane(workspace, active_target)
    }

    fn main_pane_generation(
        &self,
        workspace: &TmuxWorkspaceHandle,
    ) -> Result<Option<String>, LifecycleError> {
        self.backend
            .show_session_option(workspace, WAITAGENT_MAIN_PANE_GENERATION_OPTION)
            .map(|value| value.filter(|value| !value.is_empty()))
            .map_err(main_slot_error)
    }

    fn bump_main_pane_generation(
        &self,
        workspace: &TmuxWorkspaceHandle,
    ) -> Result<String, LifecycleError> {
        let next = self
            .main_pane_generation(workspace)?
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(0)
            + 1;
        let next = next.to_string();
        self.backend
            .set_session_option(workspace, WAITAGENT_MAIN_PANE_GENERATION_OPTION, &next)
            .map_err(main_slot_error)?;
        Ok(next)
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
            // Pane died — kill it to prevent dead-pane accumulation,
            // then clean up the stale option so a new one is created.
            let _ = self.backend.kill_pane(workspace, &pane_id);
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

    fn clear_session_pane(
        &self,
        workspace: &TmuxWorkspaceHandle,
        qualified_target: &str,
    ) -> Result<(), LifecycleError> {
        let option_name = self.session_pane_option_name(qualified_target);
        self.backend
            .set_session_option(workspace, &option_name, "")
            .map_err(main_slot_error)
    }

    fn cleanup_exited_remote_session_pane(
        &self,
        workspace: &TmuxWorkspaceHandle,
        qualified_target: &str,
        pane: Option<&TmuxPaneId>,
    ) -> Result<(), LifecycleError> {
        if let Some(pane) = pane {
            let _ = self.backend.unset_pane_hook(workspace, pane, "pane-died");
            let _ = self
                .backend
                .set_pane_option(workspace, pane, "remain-on-exit", "off");
            let _ = self.backend.kill_pane(workspace, pane);
        }
        self.clear_session_pane(workspace, qualified_target)
    }

    fn create_remote_session_pane(
        &self,
        workspace: &TmuxWorkspaceHandle,
        main_pane: &TmuxPaneId,
        current_workspace: &CurrentWorkspace,
        target: &ManagedSessionRecord,
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

    fn pane_exists(&self, workspace: &TmuxWorkspaceHandle, pane_id: &str) -> bool {
        let Ok(window) = self.backend.current_window(workspace) else {
            return false;
        };
        self.backend
            .list_panes(workspace, &window)
            .map(|panes| {
                panes
                    .into_iter()
                    .any(|pane| pane.pane_id.as_str() == pane_id)
            })
            .unwrap_or(false)
    }

    fn pane_exists_on_socket(
        &self,
        workspace: &TmuxWorkspaceHandle,
        pane_id: &str,
    ) -> Result<bool, LifecycleError> {
        let output = self
            .backend
            .run_on_socket(
                &workspace.socket_name,
                &[
                    "list-panes".to_string(),
                    "-a".to_string(),
                    "-F".to_string(),
                    "#{pane_id}".to_string(),
                ],
            )
            .map_err(main_slot_error)?;
        Ok(output.stdout.lines().any(|line| line.trim() == pane_id))
    }

    fn pane_is_live(&self, workspace: &TmuxWorkspaceHandle, pane_id: &str) -> bool {
        self.backend
            .pane_is_alive(workspace, &TmuxPaneId::new(pane_id))
            .unwrap_or(false)
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

    fn workspace_visible_targets(
        &self,
        socket_name: &str,
        session_name: &str,
        active_target: Option<&str>,
    ) -> Result<Vec<ManagedSessionRecord>, LifecycleError> {
        self.target_registry_for_socket(socket_name)?
            .visible_targets_in_workspace(socket_name, session_name, active_target)
            .map_err(main_slot_error)
    }
}

struct CurrentWorkspace {
    socket_name: String,
    session_name: String,
    workspace_dir: PathBuf,
}

struct MainPaneStateGuard<'a> {
    backend: &'a EmbeddedTmuxBackend,
    socket_name: TmuxSocketName,
    lock_name: String,
}

impl Drop for MainPaneStateGuard<'_> {
    fn drop(&mut self) {
        let _ = self.backend.run_on_socket(
            &self.socket_name,
            &[
                "wait-for".to_string(),
                "-U".to_string(),
                self.lock_name.clone(),
            ],
        );
    }
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
    let (socket_name, session_name) = target.rsplit_once(':')?;
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

fn next_remote_fallback_target(
    sessions: &[ManagedSessionRecord],
    active_target: Option<&str>,
) -> Option<ManagedSessionRecord> {
    let active_target = active_target.filter(|target| !target.is_empty());
    sessions
        .iter()
        .filter(|session| {
            session.is_target_host()
                && (session.address.transport() == &SessionTransport::RemotePeer
                    || session.address.authority_id().contains('#'))
        })
        .find(|session| {
            active_target.map_or(true, |active| session.address.qualified_target() != active)
        })
        .cloned()
}

fn workspace_shell_program(workspace_dir: &Path) -> TmuxProgram {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());
    TmuxProgram::new(shell).with_start_directory(workspace_dir)
}

fn extract_remote_authority_connect_addr(authority_id: &str) -> Option<String> {
    let (ip, port) = authority_id.split_once('#')?;
    Some(format!("{ip}:{port}"))
}

fn remote_main_slot_program(
    executable: &Path,
    current_workspace: &CurrentWorkspace,
    target: &ManagedSessionRecord,
    network: &RemoteNetworkConfig,
) -> TmuxProgram {
    let mut network = network.clone();
    if let Some(connect_addr) = extract_remote_authority_connect_addr(target.address.authority_id())
    {
        network.connect = Some(connect_addr);
    }
    TmuxProgram::new(executable.display().to_string())
        .with_args(prepend_global_network_args(
            vec![
                "__remote-main-slot".to_string(),
                "--socket-name".to_string(),
                current_workspace.socket_name.clone(),
                "--session-name".to_string(),
                current_workspace.session_name.clone(),
                "--target".to_string(),
                target.address.qualified_target(),
            ],
            &network,
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
