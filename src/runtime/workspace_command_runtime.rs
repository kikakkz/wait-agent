use crate::application::session_service::SessionService;
use crate::application::workspace_path_service::WorkspacePathService;
use crate::application::workspace_service::WorkspaceService;
use crate::cli::{
    ActivateTargetCommand, AttachCommand, DetachCommand, ListCommand, MainPaneDiedCommand,
    NewTargetCommand, ToggleFullscreenCommand, WorkspaceCommand,
};
use crate::domain::session_catalog::ManagedSessionRecord;
use crate::infra::tmux::{EmbeddedTmuxBackend, TmuxError};
use crate::lifecycle::LifecycleError;
use crate::runtime::main_slot_runtime::MainSlotRuntime;
use crate::runtime::native_pane_fullscreen_runtime::NativePaneFullscreenRuntime;
use crate::runtime::target_host_runtime::TargetHostRuntime;
use crate::runtime::workspace_entry_runtime::WorkspaceEntryRuntime;
use crate::runtime::workspace_layout_runtime::WorkspaceLayoutRuntime;
use crate::runtime::workspace_runtime::WorkspaceRuntime;
use std::io;

pub struct WorkspaceCommandRuntime {
    path_service: WorkspacePathService,
    entry_runtime: WorkspaceEntryRuntime,
    main_slot_runtime: MainSlotRuntime,
    fullscreen_runtime: NativePaneFullscreenRuntime,
    session_service: SessionService<EmbeddedTmuxBackend>,
}

impl WorkspaceCommandRuntime {
    pub fn from_build_env() -> Result<Self, LifecycleError> {
        let backend = EmbeddedTmuxBackend::from_build_env().map_err(tmux_runtime_error)?;
        let current_executable = std::env::current_exe().map_err(|error| {
            LifecycleError::Io(
                "failed to locate current waitagent executable".to_string(),
                error,
            )
        })?;
        let entry_runtime = WorkspaceEntryRuntime::new(
            WorkspaceRuntime::new(WorkspaceService::new(backend.clone())),
            WorkspaceLayoutRuntime::from_build_env()?,
        );
        let session_service = SessionService::new(backend.clone());
        let main_slot_backend = backend.clone();
        let target_host_runtime = TargetHostRuntime::from_backend(backend.clone());

        Ok(Self {
            path_service: WorkspacePathService::new(),
            entry_runtime,
            main_slot_runtime: MainSlotRuntime::new(
                main_slot_backend.clone(),
                target_host_runtime,
                WorkspaceLayoutRuntime::from_build_env()?,
                SessionService::new(main_slot_backend),
                current_executable.clone(),
            ),
            fullscreen_runtime: NativePaneFullscreenRuntime::new(
                backend.clone(),
                SessionService::new(backend.clone()),
                WorkspaceLayoutRuntime::from_build_env()?,
            ),
            session_service,
        })
    }

    pub fn run_workspace_entry(&self, _command: WorkspaceCommand) -> Result<(), LifecycleError> {
        let workspace_dir = self.resolve_workspace_dir(None)?;
        let workspace = self.entry_runtime.bootstrap_workspace(&workspace_dir)?;
        self.main_slot_runtime.ensure_initial_target_materialized(
            &workspace.workspace_handle,
            &workspace.workspace_dir,
        )?;
        self.session_service
            .attach_workspace(&workspace.workspace_handle)
            .map_err(tmux_runtime_error)
    }

    pub fn run_attach(&self, command: AttachCommand) -> Result<(), LifecycleError> {
        match attach_target_path(&command) {
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

    pub fn run_list(&self, _command: ListCommand) -> Result<(), LifecycleError> {
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
        if let Some(target) = attach_target_path(&AttachCommand {
            target: command.target.clone(),
        }) {
            let session = self.attachable_session(target)?;
            self.session_service
                .detach_session_clients(&session)
                .map_err(tmux_runtime_error)?;
            println!(
                "detached clients from {}",
                session.address.qualified_target()
            );
            return Ok(());
        }

        if std::env::var_os("TMUX").is_some() {
            self.session_service
                .detach_current_client()
                .map_err(tmux_runtime_error)?;
            return Ok(());
        }

        let session = self
            .session_service
            .resolve_default_attach_session()
            .map_err(tmux_runtime_error)?;
        self.session_service
            .detach_session_clients(&session)
            .map_err(tmux_runtime_error)?;
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
            .session_service
            .find_session(&target)
            .map_err(tmux_runtime_error)?
            .ok_or_else(|| LifecycleError::Protocol(format!("unknown tmux target `{target}`")))?;
        Ok(session)
    }
}

fn attach_target_path(command: &AttachCommand) -> Option<String> {
    command.target.as_ref().cloned()
}

fn tmux_runtime_error(error: TmuxError) -> LifecycleError {
    LifecycleError::Io(
        "tmux-native waitagent command failed".to_string(),
        io::Error::new(io::ErrorKind::Other, error.to_string()),
    )
}
