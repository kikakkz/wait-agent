use crate::application::target_registry_service::{
    DefaultTargetCatalogGateway, TargetRegistryService,
};
use crate::application::workspace_service::{BootstrappedWorkspace, WorkspaceService};
use crate::domain::session_catalog::{ManagedSessionRecord, SessionTransport};
use crate::domain::workspace::WorkspaceInstanceConfig;
use crate::infra::tmux::{EmbeddedTmuxBackend, TmuxError, TmuxSocketName};
use crate::lifecycle::LifecycleError;
use crate::runtime::remote_target_publication_runtime::RemoteTargetPublicationRuntime;
use crate::runtime::workspace_runtime::WorkspaceRuntime;
use std::io;

pub struct TargetHostRuntime {
    workspace_runtime: WorkspaceRuntime<EmbeddedTmuxBackend>,
    backend: EmbeddedTmuxBackend,
    remote_target_publication_runtime: RemoteTargetPublicationRuntime,
    target_registry: TargetRegistryService<DefaultTargetCatalogGateway>,
}

impl TargetHostRuntime {
    pub fn new(
        workspace_runtime: WorkspaceRuntime<EmbeddedTmuxBackend>,
        backend: EmbeddedTmuxBackend,
        remote_target_publication_runtime: RemoteTargetPublicationRuntime,
        target_registry: TargetRegistryService<DefaultTargetCatalogGateway>,
    ) -> Self {
        Self {
            workspace_runtime,
            backend,
            remote_target_publication_runtime,
            target_registry,
        }
    }

    pub fn from_build_env(backend: EmbeddedTmuxBackend) -> Result<Self, LifecycleError> {
        Ok(Self::new(
            WorkspaceRuntime::new(WorkspaceService::new(backend.clone())),
            backend,
            RemoteTargetPublicationRuntime::from_build_env()?,
            TargetRegistryService::new(
                DefaultTargetCatalogGateway::from_build_env().map_err(target_host_error)?,
            ),
        ))
    }

    pub fn ensure_target_host(
        &self,
        config: WorkspaceInstanceConfig,
    ) -> Result<BootstrappedWorkspace, TmuxError> {
        self.workspace_runtime.ensure_workspace_for_config(config)
    }

    pub fn refresh_published_target_session(
        &self,
        session: Option<&ManagedSessionRecord>,
    ) -> Result<(), LifecycleError> {
        let Some(session) = session.filter(|session| session.is_target_host()) else {
            return Ok(());
        };
        self.remote_target_publication_runtime
            .signal_source_session_refresh(
                session.address.server_id(),
                session.address.session_id(),
            )
    }

    pub fn close_target_session_identity(
        &self,
        target: Option<&str>,
    ) -> Result<(), LifecycleError> {
        let Some(target) = target else {
            return Ok(());
        };
        if let Some(session) = self
            .target_registry
            .find_target(target)
            .map_err(target_host_error)?
        {
            return self.close_resolved_target_session(&session);
        }
        if let Some((socket_name, session_name)) = split_qualified_target(target) {
            self.remote_target_publication_runtime
                .signal_source_session_closed(socket_name, session_name)?;
            return match self.backend.run_socket_command(
                &TmuxSocketName::new(socket_name),
                &[
                    "kill-session".to_string(),
                    "-t".to_string(),
                    session_name.to_string(),
                ],
            ) {
                Ok(()) => Ok(()),
                Err(error) if error.is_command_failure() => Ok(()),
                Err(error) => Err(target_host_error(error)),
            };
        }
        Ok(())
    }

    fn close_resolved_target_session(
        &self,
        session: &ManagedSessionRecord,
    ) -> Result<(), LifecycleError> {
        if session.address.transport() == &SessionTransport::RemotePeer || !session.is_target_host()
        {
            return Ok(());
        }
        self.remote_target_publication_runtime
            .signal_source_session_closed(
                session.address.server_id(),
                session.address.session_id(),
            )?;
        self.backend
            .run_socket_command(
                &TmuxSocketName::new(session.address.server_id()),
                &[
                    "kill-session".to_string(),
                    "-t".to_string(),
                    session.address.session_id().to_string(),
                ],
            )
            .map_err(target_host_error)
    }
}

fn split_qualified_target(target: &str) -> Option<(&str, &str)> {
    let (socket_name, session_name) = target.split_once(':')?;
    if socket_name.is_empty() || session_name.is_empty() {
        return None;
    }
    Some((socket_name, session_name))
}

fn target_host_error(error: TmuxError) -> LifecycleError {
    LifecycleError::Io(
        "tmux-native target-host command failed".to_string(),
        io::Error::new(io::ErrorKind::Other, error.to_string()),
    )
}
