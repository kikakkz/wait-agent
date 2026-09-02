// Legacy tmux-era publication runtime kept during the ratatui migration; many items are currently unused.

use crate::cli::RemoteNetworkConfig;
use crate::domain::session_catalog::ManagedSessionRecord;
use crate::infra::error_log::ERROR_LOG;
use crate::infra::remote_protocol::{ControlPlanePayload, ProtocolEnvelope};
use crate::lifecycle::LifecycleError;
use crate::process::current_executable::current_waitagent_executable;
use crate::remote::node::remote_node_session_sync_runtime::{
    LocalCatalogChangeReason, RemoteNodeSessionSyncRuntime,
};
use crate::remote::node::remote_runtime_owner_runtime::RemoteRuntimeOwnerRuntime;
use crate::remote::publication::remote_target_publication_backend::RemoteTargetPublicationBackend;

use std::io::ErrorKind;
#[cfg(unix)]
use std::io::Write;
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

mod publication_helpers;
pub(crate) use publication_helpers::*;

#[derive(Clone)]
pub struct RemoteTargetPublicationRuntime<B: RemoteTargetPublicationBackend> {
    remote_runtime_owner: RemoteRuntimeOwnerClient,
    backend: B,
    current_executable: PathBuf,
    network: RemoteNetworkConfig,
    discover_live_workspaces: bool,
}

#[derive(Clone)]
enum RemoteRuntimeOwnerClient {
    Runtime(RemoteRuntimeOwnerRuntime),
    #[cfg(test)]
    Noop,
}

// TODO(cleanup): transitional remote code, kept for Phase 8 wiring.
#[allow(dead_code)]
impl RemoteRuntimeOwnerClient {
    fn upsert_session(
        &self,
        node_id: &str,
        session: &ManagedSessionRecord,
    ) -> Result<(), LifecycleError> {
        match self {
            RemoteRuntimeOwnerClient::Runtime(runtime) => runtime.upsert_session(node_id, session),
            #[cfg(test)]
            RemoteRuntimeOwnerClient::Noop => Ok(()),
        }
    }

    fn remove_session(
        &self,
        node_id: &str,
        authority_id: &str,
        transport_session_id: &str,
    ) -> Result<(), LifecycleError> {
        match self {
            RemoteRuntimeOwnerClient::Runtime(runtime) => {
                runtime.remove_session(node_id, authority_id, transport_session_id)
            }
            #[cfg(test)]
            RemoteRuntimeOwnerClient::Noop => Ok(()),
        }
    }

    fn mark_node_offline(&self, node_id: &str) -> Result<(), LifecycleError> {
        match self {
            RemoteRuntimeOwnerClient::Runtime(runtime) => runtime.mark_node_offline(node_id),
            #[cfg(test)]
            RemoteRuntimeOwnerClient::Noop => Ok(()),
        }
    }

    fn mark_session_offline_by_source(
        &self,
        node_id: &str,
        authority_id: &str,
        transport_session_id: &str,
        source_socket_name: &str,
        authority_host_session_name: Option<&str>,
    ) -> Result<(), LifecycleError> {
        match self {
            RemoteRuntimeOwnerClient::Runtime(runtime) => runtime.mark_session_offline_by_source(
                node_id,
                authority_id,
                transport_session_id,
                source_socket_name,
                authority_host_session_name,
            ),
            #[cfg(test)]
            RemoteRuntimeOwnerClient::Noop => Ok(()),
        }
    }
}

// TODO(cleanup): transitional remote code, kept for Phase 8 wiring.
#[allow(dead_code)]
impl<B: RemoteTargetPublicationBackend> RemoteTargetPublicationRuntime<B> {
    pub fn with_network_and_backend(
        network: RemoteNetworkConfig,
        backend: B,
    ) -> Result<Self, LifecycleError> {
        Ok(Self {
            remote_runtime_owner: RemoteRuntimeOwnerClient::Runtime(
                RemoteRuntimeOwnerRuntime::from_build_env_with_network(network.clone())?,
            ),
            backend,
            current_executable: current_waitagent_executable()?,
            network,
            discover_live_workspaces: true,
        })
    }

    #[cfg(test)]
    pub fn with_network_backend_and_noop_owner(
        network: RemoteNetworkConfig,
        backend: B,
    ) -> Result<Self, LifecycleError> {
        Ok(Self {
            remote_runtime_owner: RemoteRuntimeOwnerClient::Noop,
            backend,
            current_executable: std::path::PathBuf::new(),
            network,
            discover_live_workspaces: true,
        })
    }

    #[cfg(unix)]
    pub fn shutdown_socket_sidecars(&self, socket_name: &str) -> Result<(), LifecycleError> {
        let mut first_error = None;
        if let Err(error) = self
            .signal_publication_agent_command_without_start(
                socket_name,
                PublicationAgentCommand::Stop,
            )
            .or_else(ignore_missing_publication_sidecar)
        {
            first_error.get_or_insert(error);
        }
        if let Err(error) =
            signal_publication_sender_command(socket_name, PublicationSenderCommand::Stop)
                .or_else(ignore_missing_publication_sidecar)
        {
            first_error.get_or_insert(error);
        }
        if let Err(error) =
            signal_publication_server_command(socket_name, PublicationServerCommand::Stop)
                .or_else(ignore_missing_publication_sidecar)
        {
            first_error.get_or_insert(error);
        }
        crate::infra::best_effort::remove_file(remote_target_publication_agent_socket_path(
            socket_name,
        ));
        crate::infra::best_effort::remove_file(remote_target_publication_sender_socket_path(
            socket_name,
        ));
        crate::infra::best_effort::remove_file(
            remote_target_publication_server_command_socket_path(socket_name),
        );
        crate::infra::best_effort::remove_file(remote_target_publication_socket_path(socket_name));
        if let Some(error) = first_error {
            return Err(error);
        }
        Ok(())
    }

    #[cfg(unix)]
    fn signal_publication_agent_command_without_start(
        &self,
        socket_name: &str,
        command: PublicationAgentCommand,
    ) -> Result<(), LifecycleError> {
        let mut stream =
            UnixStream::connect(remote_target_publication_agent_socket_path(socket_name))
                .map_err(remote_target_publication_error)?;
        stream
            .write_all(render_publication_agent_command(&command).as_bytes())
            .map_err(remote_target_publication_error)?;
        stream.flush().map_err(remote_target_publication_error)
    }

    pub fn apply_live_publication_envelope(
        &self,
        socket_name: &str,
        envelope: ProtocolEnvelope<ControlPlanePayload>,
    ) -> Result<(), LifecycleError> {
        let node_id = envelope.sender_id.clone();
        let remote_session = discovered_remote_session_from_envelope(&node_id, &envelope)?;
        let mut changed = false;
        if let Some(session) = remote_session.published_session {
            if is_publishable_discovered_remote_session(&session) {
                self.signal_remote_runtime_owner_upsert(&node_id, &session)?;
                changed = true;
            }
        }
        if let Some((authority_id, transport_session_id)) = remote_session.exited_session {
            self.signal_remote_runtime_owner_remove(
                &node_id,
                &authority_id,
                &transport_session_id,
            )?;
            changed = true;
        }
        if changed {
            self.refresh_live_workspace_socket(socket_name)?;
        }
        Ok(())
    }

    pub fn apply_discovered_remote_session_envelope(
        &self,
        node_id: &str,
        envelope: ProtocolEnvelope<ControlPlanePayload>,
    ) -> Result<(), LifecycleError> {
        let live_workspace_sockets = self.live_workspace_socket_names()?;
        self.apply_discovered_remote_session_envelope_for_sockets(
            node_id,
            envelope,
            &live_workspace_sockets,
        )
    }

    pub fn apply_discovered_remote_session_envelope_for_sockets(
        &self,
        node_id: &str,
        envelope: ProtocolEnvelope<ControlPlanePayload>,
        live_workspace_sockets: &[String],
    ) -> Result<(), LifecycleError> {
        let _t_total = std::time::Instant::now();
        let remote_session = discovered_remote_session_from_envelope(node_id, &envelope)?;
        if let Some(session) = remote_session.published_session {
            if is_publishable_discovered_remote_session(&session) {
                self.signal_remote_runtime_owner_upsert(node_id, &session)?;
            }
        }
        if let Some((authority_id, transport_session_id)) = remote_session.exited_session {
            let _t_remove = std::time::Instant::now();
            self.signal_remote_runtime_owner_remove(node_id, &authority_id, &transport_session_id)?;

            let _t_workspace = std::time::Instant::now();
            let target = format!("{authority_id}:{transport_session_id}");
            let _signalled = self
                .signal_remote_target_exited_to_live_workspaces(live_workspace_sockets, &target)?;
        }

        if !live_workspace_sockets.is_empty() {
            let _t_refresh = std::time::Instant::now();
            self.refresh_live_workspaces(live_workspace_sockets)?;
        }
        Ok(())
    }

    pub fn mark_discovered_remote_node_offline(&self, node_id: &str) -> Result<(), LifecycleError> {
        self.signal_remote_runtime_owner_mark_node_offline(node_id)?;
        if !self.discover_live_workspaces {
            return Ok(());
        }
        let live_workspace_sockets = self.live_workspace_socket_names()?;
        if !live_workspace_sockets.is_empty() {
            self.refresh_live_workspaces(&live_workspace_sockets)?;
        }
        Ok(())
    }

    pub fn signal_remote_node_offline(&self, node_id: &str) -> Result<(), LifecycleError> {
        self.backend.signal_remote_node_offline(node_id)
    }

    pub fn signal_remote_node_online(&self, node_id: &str) -> Result<(), LifecycleError> {
        self.backend.signal_remote_node_online(node_id)
    }

    pub fn record_inbound_remote_node_connection(
        &self,
        node_id: &str,
        host: &str,
        port: u16,
        server_can_reach_peer: bool,
    ) -> Result<(), LifecycleError> {
        self.backend.record_inbound_remote_node_connection(
            node_id,
            host,
            port,
            server_can_reach_peer,
        )
    }

    pub fn mark_source_target_offline(
        &self,
        socket_name: &str,
        session_name: &str,
        target_id: &str,
    ) -> Result<(), LifecycleError> {
        let Some((authority_id, transport_session_id)) = parse_remote_target_id(target_id) else {
            return Ok(());
        };
        let node_id = authority_id;
        self.remote_runtime_owner.mark_session_offline_by_source(
            node_id,
            authority_id,
            transport_session_id,
            socket_name,
            Some(session_name),
        )?;
        self.refresh_live_workspace_socket(socket_name)
    }

    fn signal_remote_runtime_owner_upsert(
        &self,
        node_id: &str,
        session: &ManagedSessionRecord,
    ) -> Result<(), LifecycleError> {
        let _t_upsert = std::time::Instant::now();

        let result = self.remote_runtime_owner.upsert_session(node_id, session);

        if result.is_ok() {
            if let Err(error) = self.backend.on_remote_session_upserted(node_id, session) {
                ERROR_LOG.log_error(format!(
                    "on_remote_session_upserted node={} target={} error={}",
                    node_id,
                    session.address.qualified_target(),
                    error
                ));
            }
        }
        result
    }

    fn signal_remote_runtime_owner_remove(
        &self,
        node_id: &str,
        authority_id: &str,
        transport_session_id: &str,
    ) -> Result<(), LifecycleError> {
        let _t_remove = std::time::Instant::now();
        let result =
            self.remote_runtime_owner
                .remove_session(node_id, authority_id, transport_session_id);

        if result.is_ok() {
            let target = format!("{authority_id}:{transport_session_id}");
            if let Err(error) = self.signal_remote_target_exited_to_live_workspaces(
                &self.live_workspace_socket_names()?,
                &target,
            ) {
                ERROR_LOG.log_error(format!(
                    "failed to signal workspace exit for {target}: {error}"
                ));
            }
        }
        result
    }

    fn signal_remote_runtime_owner_mark_node_offline(
        &self,
        node_id: &str,
    ) -> Result<(), LifecycleError> {
        self.remote_runtime_owner.mark_node_offline(node_id)
    }

    pub(crate) fn live_workspace_socket_names(&self) -> Result<Vec<String>, LifecycleError> {
        if !self.discover_live_workspaces {
            return Ok(Vec::new());
        }
        self.backend.live_workspace_socket_names(&self.network)
    }

    fn signal_remote_target_exited_to_live_workspaces(
        &self,
        socket_names: &[String],
        target: &str,
    ) -> Result<usize, LifecycleError> {
        let mut signalled = 0;
        for socket_name in socket_names {
            let _t_workspace = std::time::Instant::now();
            let count = self.backend.signal_remote_target_exited_to_workspace(
                socket_name,
                target,
                &self.current_executable,
            )?;
            signalled += count;
        }
        Ok(signalled)
    }

    fn refresh_live_workspaces(&self, socket_names: &[String]) -> Result<(), LifecycleError> {
        for socket_name in socket_names {
            let _t_spawn = std::time::Instant::now();
            self.backend
                .refresh_workspace_socket(socket_name, &self.current_executable)?;
        }
        Ok(())
    }

    fn refresh_live_workspace_socket(&self, socket_name: &str) -> Result<(), LifecycleError> {
        self.backend
            .refresh_workspace_socket(socket_name, &self.current_executable)
    }

    #[cfg(unix)]
    pub fn signal_source_session_closed(
        &self,
        socket_name: &str,
        session_name: &str,
    ) -> Result<(), LifecycleError> {
        let _ = signal_publication_owner_command(
            socket_name,
            session_name,
            PublicationOwnerCommand::Stop,
        );
        Ok(())
    }

    pub fn signal_local_runtime_changed(&self, socket_name: &str) -> Result<(), LifecycleError> {
        RemoteNodeSessionSyncRuntime::notify_local_catalog_changed(
            socket_name,
            &self.network,
            LocalCatalogChangeReason::LocalRuntimeChanged,
        )
    }

    pub fn ensure_publication_server_running(
        &self,
        socket_name: &str,
    ) -> Result<(), LifecycleError> {
        self.backend.ensure_publication_server_running(
            socket_name,
            &self.network,
            &self.current_executable,
        )
    }

    pub(crate) fn ensure_publication_sender_running(
        &self,
        socket_name: &str,
    ) -> Result<(), LifecycleError> {
        self.backend.ensure_publication_sender_running(
            socket_name,
            &self.network,
            &self.current_executable,
        )
    }
}

// TODO(cleanup): transitional remote code, kept for Phase 8 wiring.
#[allow(dead_code)]
pub fn remote_target_publication_socket_path(socket_name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "waitagent-remote-publication-{}.sock",
        sanitize_path_component(socket_name)
    ))
}

// TODO(cleanup): transitional remote code, kept for Phase 8 wiring.
#[allow(dead_code)]
fn parse_remote_target_id(target_id: &str) -> Option<(&str, &str)> {
    let suffix = target_id.strip_prefix("remote-peer:")?;
    suffix.split_once(':')
}

// TODO(cleanup): transitional remote code, kept for Phase 8 wiring.
#[allow(dead_code)]
fn ignore_missing_publication_sidecar(error: LifecycleError) -> Result<(), LifecycleError> {
    match &error {
        LifecycleError::Io(_, io_error)
            if matches!(
                io_error.kind(),
                ErrorKind::NotFound | ErrorKind::ConnectionRefused
            ) =>
        {
            Ok(())
        }
        _ => Err(error),
    }
}
