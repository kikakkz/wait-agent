use crate::application::session_service::SessionService;
use crate::application::target_registry_service::{
    DefaultTargetCatalogGateway, TargetRegistryService,
};
use crate::application::workspace_path_service::WorkspacePathService;
use crate::application::workspace_service::WorkspaceService;
use crate::cli::{
    ActivateTargetCommand, AttachCommand, DetachCommand, MainPaneDiedCommand, NewTargetCommand,
    RemoteNetworkConfig, ToggleFullscreenCommand,
};
use crate::domain::session_catalog::{ManagedSessionRecord, SessionTransport};
use crate::infra::tmux::{EmbeddedTmuxBackend, TmuxError};
use crate::lifecycle::LifecycleError;
use crate::runtime::main_slot_runtime::MainSlotRuntime;
use crate::runtime::native_pane_fullscreen_runtime::NativePaneFullscreenRuntime;
use crate::runtime::remote_target_publication_runtime::RemoteTargetPublicationRuntime;
use crate::runtime::target_host_runtime::TargetHostRuntime;
use crate::runtime::workspace_entry_runtime::WorkspaceEntryRuntime;
use crate::runtime::workspace_layout_runtime::WorkspaceLayoutRuntime;
use crate::runtime::workspace_runtime::WorkspaceRuntime;
use std::io;

// This runtime owns the accepted default local command path for workspace
// bootstrap, attach, target activation, fullscreen, and detach semantics.
// Event-r4 keeps these user-facing entrypoints off historical polling paths.
pub struct WorkspaceCommandRuntime {
    path_service: WorkspacePathService,
    entry_runtime: WorkspaceEntryRuntime,
    main_slot_runtime: MainSlotRuntime,
    fullscreen_runtime: NativePaneFullscreenRuntime,
    remote_target_publication_runtime: RemoteTargetPublicationRuntime,
    target_host_runtime: TargetHostRuntime,
    session_service: SessionService<EmbeddedTmuxBackend>,
    target_registry: TargetRegistryService<DefaultTargetCatalogGateway>,
}

impl WorkspaceCommandRuntime {
    pub fn from_build_env() -> Result<Self, LifecycleError> {
        Self::from_build_env_with_network(RemoteNetworkConfig::default())
    }

    pub fn from_build_env_with_network(
        network: RemoteNetworkConfig,
    ) -> Result<Self, LifecycleError> {
        let backend = EmbeddedTmuxBackend::from_build_env().map_err(tmux_runtime_error)?;
        let current_executable = std::env::current_exe().map_err(|error| {
            LifecycleError::Io(
                "failed to locate current waitagent executable".to_string(),
                error,
            )
        })?;
        let entry_runtime = WorkspaceEntryRuntime::new(
            WorkspaceRuntime::new(WorkspaceService::new(backend.clone())),
            WorkspaceLayoutRuntime::from_build_env_with_network(network.clone())?,
        );
        let session_service = SessionService::new(backend.clone());
        let target_registry = TargetRegistryService::new(
            DefaultTargetCatalogGateway::from_build_env().map_err(tmux_runtime_error)?,
        );
        let main_slot_backend = backend.clone();
        let target_host_runtime = TargetHostRuntime::from_build_env(backend.clone())?;
        let command_target_host_runtime = TargetHostRuntime::from_build_env(backend.clone())?;
        let remote_target_publication_runtime =
            RemoteTargetPublicationRuntime::from_build_env_with_network(network.clone())?;

        Ok(Self {
            path_service: WorkspacePathService::new(),
            entry_runtime,
            main_slot_runtime: MainSlotRuntime::new(
                main_slot_backend.clone(),
                target_host_runtime,
                WorkspaceLayoutRuntime::from_build_env_with_network(network.clone())?,
                TargetRegistryService::new(
                    DefaultTargetCatalogGateway::from_build_env().map_err(tmux_runtime_error)?,
                ),
                current_executable,
                network.clone(),
            ),
            fullscreen_runtime: NativePaneFullscreenRuntime::new(
                backend.clone(),
                TargetRegistryService::new(
                    DefaultTargetCatalogGateway::from_build_env().map_err(tmux_runtime_error)?,
                ),
                WorkspaceLayoutRuntime::from_build_env_with_network(network.clone())?,
            ),
            remote_target_publication_runtime,
            target_host_runtime: command_target_host_runtime,
            session_service,
            target_registry,
        })
    }

    pub fn run_workspace_entry(&self) -> Result<(), LifecycleError> {
        let workspace_dir = self.resolve_workspace_dir(None)?;
        let workspace = self.entry_runtime.bootstrap_workspace(&workspace_dir)?;
        self.main_slot_runtime.ensure_initial_target_materialized(
            &workspace.workspace_handle,
            &workspace.workspace_dir,
        )?;
        self.remote_target_publication_runtime
            .ensure_configured_publications_on_socket(
                workspace.workspace_handle.socket_name.as_str(),
            )?;
        self.session_service
            .attach_workspace(&workspace.workspace_handle)
            .map_err(tmux_runtime_error)
    }

    pub fn run_attach(&self, command: AttachCommand) -> Result<(), LifecycleError> {
        match command.target.clone() {
            Some(target) => {
                let session = self.attachable_session(target)?;
                self.session_service
                    .attach_session(&session)
                    .map_err(tmux_runtime_error)
            }
            None => self
                .session_service
                .resolve_default_attach_session()
                .map_err(tmux_runtime_error)
                .and_then(|session| {
                    self.session_service
                        .attach_session(&session)
                        .map_err(tmux_runtime_error)
                }),
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

    pub fn run_main_pane_died(&self, command: MainPaneDiedCommand) -> Result<(), LifecycleError> {
        self.main_slot_runtime.run_main_pane_died(command)
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
}

fn tmux_runtime_error(error: TmuxError) -> LifecycleError {
    LifecycleError::Io(
        "tmux-native waitagent command failed".to_string(),
        io::Error::new(io::ErrorKind::Other, error.to_string()),
    )
}
