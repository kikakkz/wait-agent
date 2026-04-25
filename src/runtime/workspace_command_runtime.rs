use crate::application::session_service::SessionService;
use crate::application::workspace_service::WorkspaceService;
use crate::cli::{
    ActivateTargetCommand, AttachCommand, DaemonCommand, DetachCommand, ListCommand,
    NewTargetCommand, WorkspaceCommand,
};
use crate::config::AppConfig;
use crate::infra::tmux::{EmbeddedTmuxBackend, TmuxError};
use crate::lifecycle::LifecycleError;
use crate::runtime::main_slot_runtime::MainSlotRuntime;
use crate::runtime::target_host_runtime::TargetHostRuntime;
use crate::runtime::workspace_bootstrap_runtime::WorkspaceBootstrapRuntime;
use crate::runtime::workspace_daemon_runtime::WorkspaceDaemonRuntime;
use crate::runtime::workspace_entry_runtime::WorkspaceEntryRuntime;
use crate::runtime::workspace_layout_runtime::WorkspaceLayoutRuntime;
use crate::runtime::workspace_runtime::WorkspaceRuntime;
use crate::terminal::TerminalSize;
use std::io;

pub struct WorkspaceCommandRuntime {
    bootstrap: WorkspaceBootstrapRuntime,
    entry_runtime: WorkspaceEntryRuntime,
    main_slot_runtime: MainSlotRuntime,
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
            bootstrap: WorkspaceBootstrapRuntime::default(),
            entry_runtime,
            main_slot_runtime: MainSlotRuntime::new(
                main_slot_backend.clone(),
                target_host_runtime,
                WorkspaceLayoutRuntime::from_build_env()?,
                SessionService::new(main_slot_backend),
                current_executable,
            ),
            session_service,
        })
    }

    pub fn run_workspace_entry(
        &self,
        _config: AppConfig,
        _command: WorkspaceCommand,
    ) -> Result<(), LifecycleError> {
        let workspace_dir = self.bootstrap.resolve_workspace_dir(None)?;
        let workspace = self.entry_runtime.bootstrap_workspace(&workspace_dir)?;
        self.session_service
            .attach_workspace(&workspace.workspace_handle)
            .map_err(tmux_runtime_error)
    }

    pub fn run_daemon(
        &self,
        config: AppConfig,
        command: DaemonCommand,
    ) -> Result<(), LifecycleError> {
        let runtime =
            config.runtime_for_workspace(command.node_id.as_deref(), command.connect.as_deref());
        let workspace_dir = self
            .bootstrap
            .resolve_workspace_dir(command.workspace_dir.as_deref())?;
        let paths = self.bootstrap.workspace_paths(&workspace_dir);
        let size = TerminalSize {
            rows: command.rows.unwrap_or(24),
            cols: command.cols.unwrap_or(80),
            pixel_width: command.pixel_width.unwrap_or(0),
            pixel_height: command.pixel_height.unwrap_or(0),
        };

        WorkspaceDaemonRuntime::start(&runtime, workspace_dir, paths, size)?.run()
    }

    pub fn run_attach(&self, command: AttachCommand) -> Result<(), LifecycleError> {
        match attach_target_path(&command)? {
            Some(target) => {
                let session = self
                    .session_service
                    .find_session(&target)
                    .map_err(tmux_runtime_error)?
                    .ok_or_else(|| {
                        LifecycleError::Protocol(format!("unknown tmux target `{target}`"))
                    })?;
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
            workspace_dir: command.workspace_dir.clone(),
            target: command.target.clone(),
        })? {
            let session = self
                .session_service
                .find_session(&target)
                .map_err(tmux_runtime_error)?
                .ok_or_else(|| {
                    LifecycleError::Protocol(format!("unknown tmux target `{target}`"))
                })?;
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
}

fn attach_target_path(command: &AttachCommand) -> Result<Option<String>, LifecycleError> {
    if let Some(target) = command.target.as_deref() {
        return Ok(Some(target.to_string()));
    }

    Ok(None)
}

fn tmux_runtime_error(error: TmuxError) -> LifecycleError {
    LifecycleError::Io(
        "tmux-native waitagent command failed".to_string(),
        io::Error::new(io::ErrorKind::Other, error.to_string()),
    )
}
