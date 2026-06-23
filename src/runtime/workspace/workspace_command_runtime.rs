use crate::application::remote_session_creation_service::{
    GrpcRemoteSessionCreationTransport, RemoteSessionCreationRequest, RemoteSessionCreationService,
};
use crate::application::session_service::SessionService;
use crate::application::target_registry_service::{
    DefaultTargetCatalogGateway, TargetRegistryService,
};
use crate::application::workspace_path_service::WorkspacePathService;
use crate::application::workspace_service::WorkspaceService;
use crate::cli::{
    ActivateTargetCommand, AttachCommand, ConnectRemoteHostCommand, DetachCommand,
    LocalTargetExitedCommand, LocalTargetHostCommand, MainPaneDiedCommand,
    NewSelectedRemoteSessionCommand, NewTargetCommand, RemoteNetworkConfig,
    RemoteNodeIngressServerCommand, RemoteTargetExitedCommand, StopCommand,
    ToggleFullscreenCommand,
};
use crate::domain::session_catalog::{ManagedSessionRecord, SessionAvailability, SessionTransport};
use crate::domain::workspace::WorkspaceInstanceId;
use crate::infra::error_log::ERROR_LOG;
use crate::infra::tmux::TmuxLayoutGateway;
use crate::infra::tmux::{
    EmbeddedTmuxBackend, TmuxError, TmuxSessionName, TmuxSocketName, TmuxWorkspaceHandle,
};
use crate::lifecycle::LifecycleError;
use crate::runtime::current_executable::current_waitagent_executable;
use crate::runtime::local_target_host_runtime::LocalTargetHostRuntime;
use crate::runtime::main_slot_runtime::MainSlotRuntime;
use crate::runtime::native_pane_fullscreen_runtime::NativePaneFullscreenRuntime;
use crate::runtime::remote_host::remote_host_connect_runtime::{
    request_from_command, RemoteHostConnectRuntime, SshRemotePortProbeFactory,
};
use crate::runtime::remote_host::remote_host_history_store::RemoteHostHistoryStore;
use crate::runtime::remote_host::ssh_remote_host_bootstrapper::SshRemoteHostBootstrapper;
use crate::runtime::remote_node_ingress_server_runtime::RemoteNodeIngressServerRuntime;
use crate::runtime::remote_node_session_sync_runtime::RemoteNodeSessionSyncRuntime;
use crate::runtime::remote_runtime_owner_runtime::RemoteRuntimeOwnerRuntime;
use crate::runtime::remote_target_publication_runtime::RemoteTargetPublicationRuntime;
use crate::runtime::remote_workspace_socket_registry_runtime::RemoteWorkspaceSocketRegistryRuntime;
use crate::runtime::target_host_runtime::TargetHostRuntime;
use crate::runtime::workspace_entry_runtime::WorkspaceEntryRuntime;
use crate::runtime::workspace_layout_runtime::WorkspaceLayoutRuntime;
use crate::runtime::workspace_runtime::WorkspaceRuntime;
use std::io;
use std::time::Instant;

const WAITAGENT_SIDEBAR_SELECTED_TARGET_OPTION: &str = "@waitagent_sidebar_selected_target";
const WAITAGENT_REMOTE_SESSION_CREATE_LOCK_PREFIX: &str = "waitagent-remote-session-create-";

// This runtime owns the accepted default local command path for workspace
// bootstrap, attach, target activation, fullscreen, and detach semantics.
// Event-r4 keeps these user-facing entrypoints off historical polling paths.
pub struct WorkspaceCommandRuntime {
    path_service: WorkspacePathService,
    entry_runtime: WorkspaceEntryRuntime,
    main_slot_runtime: MainSlotRuntime,
    local_target_host_runtime: LocalTargetHostRuntime,
    fullscreen_runtime: NativePaneFullscreenRuntime,
    remote_runtime_owner_runtime: RemoteRuntimeOwnerRuntime,
    remote_target_publication_runtime: RemoteTargetPublicationRuntime,
    remote_workspace_socket_registry_runtime: RemoteWorkspaceSocketRegistryRuntime,
    target_host_runtime: TargetHostRuntime,
    session_service: SessionService<EmbeddedTmuxBackend>,
    target_registry: TargetRegistryService<DefaultTargetCatalogGateway>,
    backend: EmbeddedTmuxBackend,
    network: RemoteNetworkConfig,
}

struct RemoteSessionCreateGuard<'a> {
    backend: &'a EmbeddedTmuxBackend,
    socket_name: TmuxSocketName,
    lock_name: String,
}

impl Drop for RemoteSessionCreateGuard<'_> {
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

impl WorkspaceCommandRuntime {
    pub fn from_build_env_with_network(
        network: RemoteNetworkConfig,
    ) -> Result<Self, LifecycleError> {
        let backend = EmbeddedTmuxBackend::from_build_env().map_err(tmux_runtime_error)?;
        let current_executable = current_waitagent_executable()?;
        let entry_runtime = WorkspaceEntryRuntime::new_with_network(
            WorkspaceRuntime::new(WorkspaceService::new(backend.clone())),
            WorkspaceLayoutRuntime::from_build_env_with_network(network.clone())?,
            network.clone(),
        );
        let session_service = SessionService::new(backend.clone());
        let target_registry = TargetRegistryService::new(
            DefaultTargetCatalogGateway::from_build_env_with_network(network.clone())
                .map_err(tmux_runtime_error)?,
        );
        let main_slot_backend = backend.clone();
        let target_host_runtime = TargetHostRuntime::from_build_env_with_network_and_executable(
            backend.clone(),
            network.clone(),
            current_executable.clone(),
        )?;
        let command_target_host_runtime =
            TargetHostRuntime::from_build_env_with_network_and_executable(
                backend.clone(),
                network.clone(),
                current_executable.clone(),
            )?;
        let remote_runtime_owner_runtime =
            RemoteRuntimeOwnerRuntime::from_build_env_with_network(network.clone())?;
        let remote_target_publication_runtime =
            RemoteTargetPublicationRuntime::from_build_env_with_network(network.clone())?;
        let remote_workspace_socket_registry_runtime =
            RemoteWorkspaceSocketRegistryRuntime::new(network.clone());
        let local_target_host_runtime = LocalTargetHostRuntime::new(
            backend.clone(),
            RemoteTargetPublicationRuntime::from_build_env_with_network(network.clone())?,
            current_executable.clone(),
            network.clone(),
        );

        Ok(Self {
            path_service: WorkspacePathService::new(),
            entry_runtime,
            main_slot_runtime: MainSlotRuntime::new(
                main_slot_backend.clone(),
                target_host_runtime,
                WorkspaceLayoutRuntime::from_build_env_with_network(network.clone())?,
                TargetRegistryService::new(
                    DefaultTargetCatalogGateway::from_build_env_with_network(network.clone())
                        .map_err(tmux_runtime_error)?,
                ),
                current_executable,
                network.clone(),
            ),
            local_target_host_runtime,
            fullscreen_runtime: NativePaneFullscreenRuntime::new(
                backend.clone(),
                TargetRegistryService::new(
                    DefaultTargetCatalogGateway::from_build_env_with_network(network.clone())
                        .map_err(tmux_runtime_error)?,
                ),
                WorkspaceLayoutRuntime::from_build_env_with_network(network.clone())?,
            ),
            remote_runtime_owner_runtime,
            remote_target_publication_runtime,
            remote_workspace_socket_registry_runtime,
            target_host_runtime: command_target_host_runtime,
            session_service,
            target_registry,
            backend,
            network,
        })
    }

    pub fn run_remote_daemon(&self) -> Result<(), LifecycleError> {
        let workspace_dir = self.resolve_workspace_dir(None)?;
        let workspace = self.entry_runtime.bootstrap_workspace(&workspace_dir)?;
        self.register_live_workspace_socket(workspace.workspace_handle.socket_name.as_str())?;
        self.remote_runtime_owner_runtime.ensure_owner_running()?;
        self.main_slot_runtime.ensure_initial_target_materialized(
            &workspace.workspace_handle,
            &workspace.workspace_dir,
        )?;
        self.remote_target_publication_runtime
            .ensure_configured_publications_on_socket(
                workspace.workspace_handle.socket_name.as_str(),
            )?;
        self.start_remote_node_ingress(workspace.workspace_handle.socket_name.as_str())?;
        self.start_remote_session_sync(workspace.workspace_handle.socket_name.as_str())?;
        while self
            .backend
            .socket_is_live(&workspace.workspace_handle.socket_name)
        {
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
        self.unregister_live_workspace_socket(workspace.workspace_handle.socket_name.as_str());
        Ok(())
    }

    pub fn run_workspace_entry(&self) -> Result<(), LifecycleError> {
        let t_entry = Instant::now();
        ERROR_LOG.log(format!(
            "[diag-newhost] workspace_entry start connect={:?}",
            self.network.connect
        ));
        let workspace_dir = self.resolve_workspace_dir(None)?;
        ERROR_LOG.log(format!(
            "[diag-newhost] workspace_entry resolve_workspace_dir done elapsed={:?}",
            t_entry.elapsed()
        ));
        let workspace = self.entry_runtime.bootstrap_workspace(&workspace_dir)?;
        ERROR_LOG.log(format!(
            "[diag-newhost] workspace_entry bootstrap_workspace socket={} session={} elapsed={:?}",
            workspace.workspace_handle.socket_name.as_str(),
            workspace.workspace_handle.session_name.as_str(),
            t_entry.elapsed()
        ));
        self.register_live_workspace_socket(workspace.workspace_handle.socket_name.as_str())?;
        self.remote_runtime_owner_runtime.ensure_owner_running()?;
        ERROR_LOG.log(format!(
            "[diag-newhost] workspace_entry remote_runtime_owner ready elapsed={:?}",
            t_entry.elapsed()
        ));
        self.main_slot_runtime.ensure_initial_target_materialized(
            &workspace.workspace_handle,
            &workspace.workspace_dir,
        )?;
        ERROR_LOG.log(format!(
            "[diag-newhost] workspace_entry initial_target_materialized elapsed={:?}",
            t_entry.elapsed()
        ));
        self.remote_target_publication_runtime
            .ensure_configured_publications_on_socket(
                workspace.workspace_handle.socket_name.as_str(),
            )?;
        ERROR_LOG.log(format!(
            "[diag-newhost] workspace_entry publications configured elapsed={:?}",
            t_entry.elapsed()
        ));
        self.start_remote_node_ingress(workspace.workspace_handle.socket_name.as_str())?;
        ERROR_LOG.log(format!(
            "[diag-newhost] workspace_entry remote_node_ingress ready elapsed={:?}",
            t_entry.elapsed()
        ));
        self.start_remote_session_sync(workspace.workspace_handle.socket_name.as_str())?;
        ERROR_LOG.log(format!(
            "[diag-newhost] workspace_entry remote_session_sync ready elapsed={:?}",
            t_entry.elapsed()
        ));
        match self
            .session_service
            .attach_workspace(&workspace.workspace_handle)
        {
            Ok(()) => Ok(()),
            Err(_error)
                if !self
                    .backend
                    .socket_is_live(&workspace.workspace_handle.socket_name) =>
            {
                Ok(())
            }
            Err(error) => Err(tmux_runtime_error(error)),
        }
    }

    pub fn run_attach(&self, command: AttachCommand) -> Result<(), LifecycleError> {
        match command.target.clone() {
            Some(target) => {
                let session = self.attachable_session(target)?;
                self.register_live_workspace_socket(session.address.server_id())?;
                self.remote_runtime_owner_runtime.ensure_owner_running()?;
                self.start_remote_node_ingress(session.address.server_id())?;
                self.start_remote_session_sync(session.address.server_id())?;
                self.session_service
                    .attach_session(&session)
                    .map_err(tmux_runtime_error)
            }
            None => {
                let session = self
                    .session_service
                    .resolve_default_attach_session()
                    .map_err(tmux_runtime_error)?;
                self.register_live_workspace_socket(session.address.server_id())?;
                self.remote_runtime_owner_runtime.ensure_owner_running()?;
                self.start_remote_node_ingress(session.address.server_id())?;
                self.start_remote_session_sync(session.address.server_id())?;
                self.session_service
                    .attach_session(&session)
                    .map_err(tmux_runtime_error)
            }
        }
    }

    pub fn run_activate_target(
        &self,
        command: ActivateTargetCommand,
    ) -> Result<(), LifecycleError> {
        self.main_slot_runtime.run_activate_target(command)
    }

    pub fn run_new_target(&self, command: NewTargetCommand) -> Result<(), LifecycleError> {
        self.main_slot_runtime.run_new_target(command)
    }

    pub fn run_new_selected_remote_session(
        &self,
        command: NewSelectedRemoteSessionCommand,
    ) -> Result<(), LifecycleError> {
        let result = self.create_selected_remote_session(command.clone());
        if let Err(error) = &result {
            self.display_remote_session_creation_error(&command, error);
        }
        result
    }

    fn create_selected_remote_session(
        &self,
        command: NewSelectedRemoteSessionCommand,
    ) -> Result<(), LifecycleError> {
        let workspace =
            workspace_handle(&command.current_socket_name, &command.current_session_name);
        let _create_guard = self.claim_remote_session_create(&workspace)?;
        let selected_target = self.selected_sidebar_target(&command)?;
        let selected_session = self
            .target_registry
            .find_target(&selected_target)
            .map_err(tmux_runtime_error)?
            .ok_or_else(|| {
                LifecycleError::Protocol(format!(
                    "selected target `{selected_target}` is no longer available"
                ))
            })?;
        if selected_session.address.transport() == &SessionTransport::LocalTmux {
            return Err(LifecycleError::Protocol(
                "selected target is local; use Ctrl-N for a local session".to_string(),
            ));
        }
        if selected_session.availability != SessionAvailability::Online {
            return Err(LifecycleError::Protocol(format!(
                "selected remote target `{}` is {}",
                selected_session.address.qualified_target(),
                selected_session.availability.as_str()
            )));
        }

        let service = RemoteSessionCreationService::new(
            GrpcRemoteSessionCreationTransport::new(self.network.clone()),
            self.target_registry.clone(),
        );
        let created = service
            .create_session(RemoteSessionCreationRequest {
                authority_node_id: selected_session.address.authority_id().to_string(),
                cwd_hint: selected_session
                    .current_path
                    .clone()
                    .or_else(|| selected_session.workspace_dir.clone()),
                cols: 0,
                rows: 0,
            })
            .map_err(|error| LifecycleError::Protocol(error.to_string()))?;
        self.remote_runtime_owner_runtime
            .upsert_session(created.address.authority_id(), &created)?;
        let qualified_target = created.address.qualified_target();
        self.backend
            .set_session_option(
                &workspace,
                WAITAGENT_SIDEBAR_SELECTED_TARGET_OPTION,
                &qualified_target,
            )
            .map_err(tmux_runtime_error)?;
        self.refresh_registered_remote_session(&command.current_socket_name)?;
        self.main_slot_runtime.run_activate_session_record(
            &command.current_socket_name,
            &command.current_session_name,
            &created,
        )
    }

    fn refresh_registered_remote_session(&self, socket_name: &str) -> Result<(), LifecycleError> {
        self.remote_target_publication_runtime
            .ensure_configured_publications_on_socket(socket_name)?;
        WorkspaceLayoutRuntime::from_build_env_with_network(self.network.clone())?
            .run_chrome_refresh_on_socket(socket_name)
    }

    fn claim_remote_session_create<'a>(
        &'a self,
        workspace: &TmuxWorkspaceHandle,
    ) -> Result<RemoteSessionCreateGuard<'a>, LifecycleError> {
        let lock_name = format!(
            "{WAITAGENT_REMOTE_SESSION_CREATE_LOCK_PREFIX}{}",
            workspace.session_name.as_str()
        );
        self.backend
            .run_on_socket(
                &workspace.socket_name,
                &["wait-for".to_string(), "-L".to_string(), lock_name.clone()],
            )
            .map_err(tmux_runtime_error)?;
        Ok(RemoteSessionCreateGuard {
            backend: &self.backend,
            socket_name: TmuxSocketName::new(workspace.socket_name.as_str()),
            lock_name,
        })
    }

    fn display_remote_session_creation_error(
        &self,
        command: &NewSelectedRemoteSessionCommand,
        error: &LifecycleError,
    ) {
        let workspace =
            workspace_handle(&command.current_socket_name, &command.current_session_name);
        let message = format!("Ctrl-S: {error}");
        let _ = self.backend.run_socket_command(
            &workspace.socket_name,
            &[
                "display-message".to_string(),
                "-t".to_string(),
                workspace.session_name.as_str().to_string(),
                message,
            ],
        );
    }

    fn selected_sidebar_target(
        &self,
        command: &NewSelectedRemoteSessionCommand,
    ) -> Result<String, LifecycleError> {
        let selected = self
            .backend
            .show_session_option(
                &workspace_handle(&command.current_socket_name, &command.current_session_name),
                WAITAGENT_SIDEBAR_SELECTED_TARGET_OPTION,
            )
            .map_err(tmux_runtime_error)?
            .unwrap_or_default();
        let selected = selected.trim();
        if selected.is_empty() {
            return Err(LifecycleError::Protocol(
                "no remote target is selected in the session sidebar".to_string(),
            ));
        }
        Ok(selected.to_string())
    }

    pub fn run_connect_remote_host(
        &self,
        command: ConnectRemoteHostCommand,
    ) -> Result<(), LifecycleError> {
        let cwd_hint = Some(self.resolve_workspace_dir(None)?);
        let request = request_from_command(
            &command,
            self.network.advertised_public_endpoint_label(),
            cwd_hint,
        )?;
        let catalog = TargetRegistryService::new(
            DefaultTargetCatalogGateway::from_build_env_with_network(self.network.clone())
                .map_err(tmux_runtime_error)?,
        );
        let runtime = RemoteHostConnectRuntime::new(
            RemoteHostHistoryStore::new(RemoteHostHistoryStore::default_path()),
            SshRemotePortProbeFactory,
            SshRemoteHostBootstrapper::default(),
            catalog.clone(),
            RemoteSessionCreationService::new(
                GrpcRemoteSessionCreationTransport::new(self.network.clone()),
                catalog,
            ),
        );
        let outcome = runtime.connect(request)?;
        self.remote_runtime_owner_runtime.upsert_session(
            outcome.created_target.address.authority_id(),
            &outcome.created_target,
        )?;
        let workspace =
            workspace_handle(&command.current_socket_name, &command.current_session_name);
        self.backend
            .set_session_option(
                &workspace,
                WAITAGENT_SIDEBAR_SELECTED_TARGET_OPTION,
                &outcome.created_target.address.qualified_target(),
            )
            .map_err(tmux_runtime_error)?;
        self.refresh_registered_remote_session(&command.current_socket_name)?;
        self.main_slot_runtime.run_activate_session_record(
            &command.current_socket_name,
            &command.current_session_name,
            &outcome.created_target,
        )
    }

    pub fn run_local_target_host(
        &self,
        command: LocalTargetHostCommand,
    ) -> Result<(), LifecycleError> {
        self.local_target_host_runtime.run_host(command)
    }

    pub fn run_local_target_exited(
        &self,
        command: LocalTargetExitedCommand,
    ) -> Result<(), LifecycleError> {
        self.local_target_host_runtime.run_target_exited(command)
    }

    pub fn run_main_pane_died(&self, command: MainPaneDiedCommand) -> Result<(), LifecycleError> {
        self.main_slot_runtime.run_main_pane_died(command)
    }

    pub fn run_remote_target_exited(
        &self,
        command: RemoteTargetExitedCommand,
    ) -> Result<(), LifecycleError> {
        self.main_slot_runtime.run_remote_target_exited(command)
    }

    pub fn run_toggle_fullscreen(
        &self,
        command: ToggleFullscreenCommand,
    ) -> Result<(), LifecycleError> {
        self.fullscreen_runtime.run_toggle(command)
    }

    pub fn run_list(&self) -> Result<(), LifecycleError> {
        let sessions = self
            .session_service
            .list_sessions()
            .map_err(tmux_runtime_error)?;
        if sessions.is_empty() {
            println!("no waitagent tmux sessions running");
            return Ok(());
        }

        for session in sessions {
            println!("{}", session.summary_line());
        }
        Ok(())
    }

    pub fn run_detach(&self, command: DetachCommand) -> Result<(), LifecycleError> {
        if let Some(target) = command.target.clone() {
            let session = self.attachable_session(target)?;
            self.session_service
                .detach_session_clients(&session)
                .map_err(tmux_runtime_error)?;
            self.target_host_runtime
                .refresh_published_target_session(Some(&session))?;
            println!(
                "detached clients from {}",
                session.address.qualified_target()
            );
            return Ok(());
        }

        if std::env::var_os("TMUX").is_some() {
            let session = self.session_service.current_client_session().ok().flatten();
            self.session_service
                .detach_current_client()
                .map_err(tmux_runtime_error)?;
            self.target_host_runtime
                .refresh_published_target_session(session.as_ref())?;
            return Ok(());
        }

        let session = self
            .session_service
            .resolve_default_attach_session()
            .map_err(tmux_runtime_error)?;
        self.session_service
            .detach_session_clients(&session)
            .map_err(tmux_runtime_error)?;
        self.target_host_runtime
            .refresh_published_target_session(Some(&session))?;
        println!(
            "detached clients from {}",
            session.address.qualified_target()
        );
        Ok(())
    }

    pub fn run_stop(&self, command: StopCommand) -> Result<(), LifecycleError> {
        let socket_name = if let Some(target) = command.target.clone() {
            let session = self.attachable_session(target)?;
            TmuxSocketName::new(session.address.server_id())
        } else if std::env::var_os("TMUX").is_some() {
            let session = self
                .session_service
                .current_client_session()
                .map_err(tmux_runtime_error)?
                .ok_or_else(|| {
                    LifecycleError::Protocol(
                        "could not determine current session from TMUX environment".to_string(),
                    )
                })?;
            TmuxSocketName::new(session.address.server_id())
        } else {
            let session = self
                .session_service
                .resolve_default_attach_session()
                .map_err(tmux_runtime_error)?;
            TmuxSocketName::new(session.address.server_id())
        };

        self.session_service
            .kill_server(&socket_name)
            .map_err(tmux_runtime_error)?;
        self.unregister_live_workspace_socket(socket_name.as_str());
        println!(
            "stopped waitagent server on socket `{}`",
            socket_name.as_str()
        );
        Ok(())
    }

    fn resolve_workspace_dir(
        &self,
        value: Option<&str>,
    ) -> Result<std::path::PathBuf, LifecycleError> {
        self.path_service
            .resolve_workspace_dir(value)
            .map_err(|error| {
                LifecycleError::Io(
                    "failed to canonicalize workspace directory".to_string(),
                    error,
                )
            })
    }

    fn attachable_session(&self, target: String) -> Result<ManagedSessionRecord, LifecycleError> {
        let session = self
            .target_registry
            .find_target(&target)
            .map_err(tmux_runtime_error)?
            .ok_or_else(|| LifecycleError::Protocol(format!("unknown tmux target `{target}`")))?;
        if session.address.transport() != &SessionTransport::LocalTmux {
            return Err(LifecycleError::Protocol(format!(
                "target `{target}` is remote and cannot be attached directly; open it from the workspace sidebar or footer instead"
            )));
        }
        Ok(session)
    }

    fn register_live_workspace_socket(&self, socket_name: &str) -> Result<(), LifecycleError> {
        self.remote_workspace_socket_registry_runtime
            .register_workspace_socket(socket_name)
    }

    fn unregister_live_workspace_socket(&self, socket_name: &str) {
        if let Err(error) = self
            .remote_workspace_socket_registry_runtime
            .unregister_workspace_socket(socket_name)
        {
            ERROR_LOG.log(format!(
                "[diag-exit] workspace_socket_registry_unregister_failed socket={} error={}",
                socket_name, error
            ));
        }
    }

    fn start_remote_session_sync(&self, socket_name: &str) -> Result<(), LifecycleError> {
        if self.network.connect.is_none() {
            return Ok(());
        }
        RemoteNodeSessionSyncRuntime::ensure_owner_running(socket_name, &self.network)?;
        Ok(())
    }

    fn start_remote_node_ingress(&self, socket_name: &str) -> Result<(), LifecycleError> {
        RemoteNodeIngressServerRuntime::ensure_owner_running(socket_name, &self.network)
    }

    pub fn run_remote_node_ingress_server(
        &self,
        command: RemoteNodeIngressServerCommand,
    ) -> Result<(), LifecycleError> {
        RemoteNodeIngressServerRuntime::from_build_env_with_network_and_socket(
            self.network.clone(),
            command.socket_name,
        )?
        .run_owner()
    }

    pub fn network_config(&self) -> RemoteNetworkConfig {
        self.network.clone()
    }
}

fn workspace_handle(socket_name: &str, session_name: &str) -> TmuxWorkspaceHandle {
    TmuxWorkspaceHandle {
        workspace_id: WorkspaceInstanceId::new(session_name),
        socket_name: TmuxSocketName::new(socket_name),
        session_name: TmuxSessionName::new(session_name),
    }
}

fn tmux_runtime_error(error: TmuxError) -> LifecycleError {
    LifecycleError::Io(
        "tmux-native waitagent command failed".to_string(),
        io::Error::new(io::ErrorKind::Other, error.to_string()),
    )
}
