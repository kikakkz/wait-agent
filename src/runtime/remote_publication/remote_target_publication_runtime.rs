use crate::cli::{
    RemoteNetworkConfig, RemoteTargetBindPublicationCommand, RemoteTargetPublicationAgentCommand,
    RemoteTargetPublicationOwnerCommand, RemoteTargetPublicationServerCommand,
    RemoteTargetReconcilePublicationsCommand, RemoteTargetUnbindPublicationCommand,
    SocketLifecycleHookCommand,
};
use crate::domain::session_catalog::{ManagedSessionRecord, SessionAvailability, SessionTransport};
use crate::infra::error_log::ERROR_LOG;
use crate::infra::remote_protocol::{ControlPlanePayload, NodeSessionChannel, ProtocolEnvelope};
use crate::infra::remote_transport_codec::read_node_session_envelope;
use crate::infra::tmux::{
    EmbeddedTmuxBackend, RemoteTargetPublicationBinding, TmuxSessionGateway, TmuxSocketName,
};
use crate::lifecycle::LifecycleError;
use crate::runtime::current_executable::current_waitagent_executable;
use crate::runtime::network_state_runtime::recover_network_config_for_socket;
use crate::runtime::remote_node_transport_runtime::{read_client_hello, write_server_hello};
use crate::runtime::remote_runtime_owner_runtime::RemoteRuntimeOwnerRuntime;
use crate::runtime::remote_target_publication_transport_runtime::remote_target_publication_socket_path;
use crate::runtime::remote_workspace_socket_registry_runtime::{
    live_workspace_socket_names_for_network, retain_live_workspace_socket_names_for_network,
    RemoteWorkspaceSocketRegistryRuntime,
};
use crate::runtime::sidecar_process_runtime::spawn_waitagent_sidecar;

use std::fs;
use std::io::Write;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::thread;

mod publication_helpers;
pub(crate) use publication_helpers::*;

#[derive(Clone)]
pub struct RemoteTargetPublicationRuntime {
    remote_runtime_owner: RemoteRuntimeOwnerClient,
    local_tmux: EmbeddedTmuxBackend,
    current_executable: PathBuf,
    network: RemoteNetworkConfig,
}

#[derive(Clone)]
enum RemoteRuntimeOwnerClient {
    Runtime(RemoteRuntimeOwnerRuntime),
    #[cfg(test)]
    Noop,
}

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
}

impl RemoteTargetPublicationRuntime {
    #[cfg(test)]
    pub fn from_build_env() -> Result<Self, LifecycleError> {
        Self::from_build_env_with_network(RemoteNetworkConfig::default())
    }

    pub fn from_build_env_with_network(
        network: RemoteNetworkConfig,
    ) -> Result<Self, LifecycleError> {
        Ok(Self {
            remote_runtime_owner: RemoteRuntimeOwnerClient::Runtime(
                RemoteRuntimeOwnerRuntime::from_build_env_with_network(network.clone())?,
            ),
            local_tmux: EmbeddedTmuxBackend::from_build_env()
                .map_err(remote_target_publication_error)?,
            current_executable: current_waitagent_executable()?,
            network,
        })
    }

    #[cfg(test)]
    pub(crate) fn new_for_route_tests_without_remote_runtime_owner() -> Result<Self, LifecycleError>
    {
        Ok(Self {
            remote_runtime_owner: RemoteRuntimeOwnerClient::Noop,
            local_tmux: EmbeddedTmuxBackend::from_build_env()
                .map_err(remote_target_publication_error)?,
            current_executable: current_waitagent_executable()?,
            network: RemoteNetworkConfig::default(),
        })
    }

    pub fn run_publication_server(
        &self,
        command: RemoteTargetPublicationServerCommand,
    ) -> Result<(), LifecycleError> {
        let socket_path = remote_target_publication_socket_path(&command.socket_name);
        if socket_path.exists() {
            let _ = fs::remove_file(&socket_path);
        }
        let listener = UnixListener::bind(&socket_path).map_err(remote_target_publication_error)?;
        for accepted in listener.incoming() {
            let Ok(mut stream) = accepted else {
                break;
            };
            let source_socket_name = command.socket_name.clone();
            let _current_executable = self.current_executable.clone();
            thread::spawn(move || {
                if read_client_hello(&mut stream).is_err() {
                    return;
                }
                if write_server_hello(&mut stream, "waitagent-publication").is_err() {
                    return;
                }
                while let Ok(session_envelope) = read_node_session_envelope(&mut stream) {
                    if session_envelope.channel != NodeSessionChannel::Publication {
                        break;
                    }
                    let _changed = match apply_publication_envelope(
                        &source_socket_name,
                        &session_envelope.envelope,
                    ) {
                        Ok(changed) => changed,
                        Err(_) => break,
                    };
                }
            });
        }
        Ok(())
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
        let t_total = std::time::Instant::now();
        let remote_session = discovered_remote_session_from_envelope(node_id, &envelope)?;
        if let Some(session) = remote_session.published_session {
            if is_publishable_discovered_remote_session(&session) {
                self.signal_remote_runtime_owner_upsert(node_id, &session)?;
            }
        }
        if let Some((authority_id, transport_session_id)) = remote_session.exited_session {
            ERROR_LOG.log_exit_latency(format!(
                "[diag-exit] publication_apply_exit_start node={} authority={} session={} stage=publication_apply",
                node_id, authority_id, transport_session_id
            ));
            let t_remove = std::time::Instant::now();
            self.signal_remote_runtime_owner_remove(node_id, &authority_id, &transport_session_id)?;
            ERROR_LOG.log_exit_latency(format!(
                "[diag-exit] publication_apply_remove node={} authority={} session={} elapsed={:?} total={:?} stage=publication_apply",
                node_id,
                authority_id,
                transport_session_id,
                t_remove.elapsed(),
                t_total.elapsed()
            ));
            let t_workspace = std::time::Instant::now();
            let target = format!("{authority_id}:{transport_session_id}");
            let signalled = self
                .signal_remote_target_exited_to_live_workspaces(live_workspace_sockets, &target)?;
            ERROR_LOG.log_exit_latency(format!(
                "[diag-exit] publication_apply_workspace_signal node={} target={} signalled={} elapsed={:?} total={:?} stage=publication_apply",
                node_id,
                target,
                signalled,
                t_workspace.elapsed(),
                t_total.elapsed()
            ));
        }
        ERROR_LOG.log_exit_latency(format!(
            "[diag-exit] publication_apply_live_sockets node={} count={} elapsed={:?} total={:?} stage=publication_apply",
            node_id,
            live_workspace_sockets.len(),
            std::time::Duration::ZERO,
            t_total.elapsed()
        ));
        if !live_workspace_sockets.is_empty() {
            let t_refresh = std::time::Instant::now();
            self.refresh_live_workspaces(live_workspace_sockets)?;
            ERROR_LOG.log_exit_latency(format!(
                "[diag-exit] publication_apply_refresh node={} sockets={:?} elapsed={:?} total={:?} stage=publication_apply",
                node_id,
                live_workspace_sockets,
                t_refresh.elapsed(),
                t_total.elapsed()
            ));
        }
        Ok(())
    }

    pub fn mark_discovered_remote_node_offline(&self, node_id: &str) -> Result<(), LifecycleError> {
        self.signal_remote_runtime_owner_mark_node_offline(node_id)?;
        let live_workspace_sockets = self.live_workspace_socket_names()?;
        if !live_workspace_sockets.is_empty() {
            self.refresh_live_workspaces(&live_workspace_sockets)?;
        }
        Ok(())
    }

    pub fn mark_source_target_offline(
        &self,
        socket_name: &str,
        session_name: &str,
        target_id: &str,
    ) -> Result<(), LifecycleError> {
        let store = crate::infra::published_target_store::PublishedTargetStore::default();
        if mark_target_offline_in_store(&store, socket_name, session_name, target_id)? {
            self.refresh_live_workspace_socket(socket_name)?;
        }
        Ok(())
    }

    pub fn run_publication_agent(
        &self,
        command: RemoteTargetPublicationAgentCommand,
    ) -> Result<(), LifecycleError> {
        self.ensure_publication_server_running(&command.socket_name)?;
        let socket_path = remote_target_publication_agent_socket_path(&command.socket_name);
        if socket_path.exists() {
            let _ = fs::remove_file(&socket_path);
        }
        let listener = UnixListener::bind(&socket_path).map_err(remote_target_publication_error)?;
        for accepted in listener.incoming() {
            let Ok(mut stream) = accepted else {
                break;
            };
            let Ok(first_command) = read_publication_agent_command(&mut stream) else {
                continue;
            };
            let mut commands = vec![first_command];
            drain_pending_publication_agent_commands(&listener, &mut commands)?;
            for agent_command in commands {
                self.process_publication_agent_command(&command.socket_name, agent_command)?;
            }
        }
        Ok(())
    }

    fn signal_remote_runtime_owner_upsert(
        &self,
        node_id: &str,
        session: &ManagedSessionRecord,
    ) -> Result<(), LifecycleError> {
        let t_upsert = std::time::Instant::now();
        ERROR_LOG.log(format!(
            "[diag-sync] upsert discovered remote session node={} target={}",
            node_id,
            session.address.qualified_target()
        ));
        let result = self.remote_runtime_owner.upsert_session(node_id, session);
        ERROR_LOG.log(format!(
            "[diag-newhost] remote_runtime_owner_upsert node={} target={} ok={} elapsed={:?}",
            node_id,
            session.address.qualified_target(),
            result.is_ok(),
            t_upsert.elapsed()
        ));
        result
    }

    fn signal_remote_runtime_owner_remove(
        &self,
        node_id: &str,
        authority_id: &str,
        transport_session_id: &str,
    ) -> Result<(), LifecycleError> {
        let t_remove = std::time::Instant::now();
        let result =
            self.remote_runtime_owner
                .remove_session(node_id, authority_id, transport_session_id);
        ERROR_LOG.log_exit_latency(format!(
            "[diag-exit] remote_runtime_owner_remove node={} authority={} session={} ok={} elapsed={:?} stage=publication_apply",
            node_id,
            authority_id,
            transport_session_id,
            result.is_ok(),
            t_remove.elapsed()
        ));
        result
    }

    fn signal_remote_runtime_owner_mark_node_offline(
        &self,
        node_id: &str,
    ) -> Result<(), LifecycleError> {
        self.remote_runtime_owner.mark_node_offline(node_id)
    }

    pub(crate) fn live_workspace_socket_names(&self) -> Result<Vec<String>, LifecycleError> {
        let registered_sockets = live_workspace_socket_names_for_network(&self.network)?;
        if !registered_sockets.is_empty()
            || RemoteWorkspaceSocketRegistryRuntime::new(self.network.clone()).registry_exists()
        {
            let live_sockets =
                retain_live_workspace_socket_names_for_network(&self.network, |socket_name| {
                    self.socket_is_live(socket_name)
                })?;
            ERROR_LOG.log_exit_latency(format!(
                "[diag-exit] publication_live_sockets_registry count={} live={} stage=publication_apply",
                registered_sockets.len(),
                live_sockets.len()
            ));
            return Ok(live_sockets);
        }
        ERROR_LOG.log_exit_latency(
            "[diag-exit] publication_live_sockets_registry_empty fallback=tmux_scan stage=publication_apply"
                .to_string(),
        );
        let mut all_sessions = Vec::new();
        if let Ok(managed_sockets) = self.local_tmux.discover_waitagent_sockets() {
            for socket in &managed_sockets {
                // Only include sockets that belong to this waitagent instance.
                // Each socket stores its owner's network port in a tmux global
                // option, so we filter out sockets created by other waitagent
                // processes on the same machine.
                if let Some(config) =
                    recover_network_config_for_socket(&self.local_tmux, socket.as_str())
                {
                    if config.port != self.network.port {
                        continue;
                    }
                }
                if let Ok(sessions) = self.local_tmux.list_sessions_on_socket(socket) {
                    all_sessions.extend(sessions);
                }
            }
        }
        Ok(live_workspace_socket_names_from_sessions(&all_sessions))
    }

    fn signal_remote_target_exited_to_live_workspaces(
        &self,
        socket_names: &[String],
        target: &str,
    ) -> Result<usize, LifecycleError> {
        let mut signalled = 0;
        for socket_name in socket_names {
            let socket = TmuxSocketName::new(socket_name);
            if !self.local_tmux.socket_is_live(&socket) {
                continue;
            }
            let Ok(sessions) = self.local_tmux.list_sessions_on_socket(&socket) else {
                continue;
            };
            for session in sessions
                .into_iter()
                .filter(|session| session.is_workspace_chrome())
            {
                let t_spawn = std::time::Instant::now();
                spawn_waitagent_sidecar(
                    &self.current_executable,
                    remote_target_exited_args(socket_name, session.address.session_id(), target),
                )
                .map_err(remote_target_publication_error)?;
                signalled += 1;
                ERROR_LOG.log_exit_latency(format!(
                    "[diag-exit] publication_workspace_exit_spawn socket={} session={} target={} elapsed={:?} stage=publication_apply",
                    socket_name,
                    session.address.session_id(),
                    target,
                    t_spawn.elapsed()
                ));
            }
        }
        Ok(signalled)
    }

    fn refresh_live_workspaces(&self, socket_names: &[String]) -> Result<(), LifecycleError> {
        for socket_name in socket_names {
            if !self.socket_is_live(socket_name) {
                continue;
            }
            let t_spawn = std::time::Instant::now();
            spawn_socket_chrome_refresh(&self.current_executable, socket_name)?;
            ERROR_LOG.log_exit_latency(format!(
                "[diag-exit] publication_refresh_spawn socket={} elapsed={:?} stage=publication_apply",
                socket_name,
                t_spawn.elapsed()
            ));
        }
        Ok(())
    }

    fn refresh_live_workspace_socket(&self, socket_name: &str) -> Result<(), LifecycleError> {
        if !self.socket_is_live(socket_name) {
            return Ok(());
        }
        spawn_socket_chrome_refresh(&self.current_executable, socket_name)
    }

    pub fn run_publication_owner(
        &self,
        command: RemoteTargetPublicationOwnerCommand,
    ) -> Result<(), LifecycleError> {
        self.ensure_publication_server_running(&command.socket_name)?;
        self.ensure_publication_sender_running(&command.socket_name)?;
        let socket_path = remote_target_publication_owner_socket_path(
            &command.socket_name,
            &command.target_session_name,
        );
        if socket_path.exists() {
            let _ = fs::remove_file(&socket_path);
        }
        let listener = UnixListener::bind(&socket_path).map_err(remote_target_publication_error)?;
        listener
            .set_nonblocking(true)
            .map_err(remote_target_publication_error)?;
        let mut last_snapshot: Option<PublicationOwnerSnapshot> = None;

        loop {
            let owner_drain = drain_publication_owner_commands(&listener)?;

            let binding = self.find_remote_publication_binding_on_socket(
                &command.socket_name,
                &command.target_session_name,
            )?;
            let Some(binding) = binding else {
                if let Some(previous) = last_snapshot.take() {
                    signal_publication_target_exited(
                        &command.socket_name,
                        &previous.authority_id,
                        &previous.transport_session_id,
                        Some(&command.target_session_name),
                    )?;
                }
                break;
            };

            let session = self
                .local_tmux
                .list_sessions_on_socket(&TmuxSocketName::new(&command.socket_name))
                .map_err(remote_target_publication_error)?
                .into_iter()
                .find(|session| {
                    session.address.session_id() == command.target_session_name
                        && session.address.transport() == &SessionTransport::LocalTmux
                        && session.is_target_host()
                });

            let Some(session) = session else {
                if let Some(previous) = last_snapshot.take() {
                    signal_publication_target_exited(
                        &command.socket_name,
                        &previous.authority_id,
                        &previous.transport_session_id,
                        Some(&command.target_session_name),
                    )?;
                }
                break;
            };

            if session.availability == SessionAvailability::Exited {
                let snapshot = publication_owner_snapshot(&binding, &session);
                signal_publication_target_exited(
                    &command.socket_name,
                    &snapshot.authority_id,
                    &snapshot.transport_session_id,
                    Some(&command.target_session_name),
                )?;
                break;
            }

            if owner_drain.stop_requested {
                let snapshot = publication_owner_snapshot(&binding, &session);
                signal_publication_target_exited(
                    &command.socket_name,
                    &snapshot.authority_id,
                    &snapshot.transport_session_id,
                    Some(&command.target_session_name),
                )?;
                break;
            }

            let snapshot = publication_owner_snapshot(&binding, &session);
            if let Some(previous) = last_snapshot.as_ref() {
                if publication_target_identity_changed(previous, &snapshot) {
                    signal_publication_target_exited(
                        &command.socket_name,
                        &previous.authority_id,
                        &previous.transport_session_id,
                        Some(&command.target_session_name),
                    )?;
                }
            }
            if owner_drain.refresh_requested || last_snapshot.as_ref() != Some(&snapshot) {
                let published = published_remote_target_from_local(&binding, &session);
                signal_publication_target_published(
                    &command.socket_name,
                    &binding.authority_id,
                    &published,
                    Some(&command.target_session_name),
                )?;
                last_snapshot = Some(snapshot);
            }

            thread::sleep(PUBLICATION_OWNER_POLL_INTERVAL);
        }

        let _ = fs::remove_file(socket_path);
        Ok(())
    }

    pub fn run_bind_publication(
        &self,
        command: RemoteTargetBindPublicationCommand,
    ) -> Result<(), LifecycleError> {
        self.ensure_publication_hooks_on_socket(&command.socket_name)?;
        bind_publication_on_socket(
            &self.local_tmux,
            &command.socket_name,
            &command.target_session_name,
            &command.authority_id,
            &command.transport_session_id,
            command.selector.as_deref(),
        )
        .map_err(remote_target_publication_error)?;
        self.ensure_publication_owner_running(&command.socket_name, &command.target_session_name)?;
        self.signal_publication_owner_command(
            &command.socket_name,
            &command.target_session_name,
            PublicationOwnerCommand::Refresh,
        )
    }

    pub fn run_unbind_publication(
        &self,
        command: RemoteTargetUnbindPublicationCommand,
    ) -> Result<(), LifecycleError> {
        self.ensure_publication_hooks_on_socket(&command.socket_name)?;
        let owner_stopped = self
            .signal_publication_owner_command(
                &command.socket_name,
                &command.target_session_name,
                PublicationOwnerCommand::Stop,
            )
            .is_ok();
        unbind_publication_on_socket(
            &self.local_tmux,
            &command.socket_name,
            &command.target_session_name,
        )
        .map_err(remote_target_publication_error)?;
        if owner_stopped {
            return Ok(());
        }
        self.signal_source_session_closed(&command.socket_name, &command.target_session_name)
    }

    pub fn run_reconcile_publications(
        &self,
        command: RemoteTargetReconcilePublicationsCommand,
    ) -> Result<(), LifecycleError> {
        self.signal_publication_reconcile(&command.socket_name)
    }

    pub fn signal_source_session_closed(
        &self,
        socket_name: &str,
        session_name: &str,
    ) -> Result<(), LifecycleError> {
        let _ = self.signal_publication_owner_command(
            socket_name,
            session_name,
            PublicationOwnerCommand::Stop,
        );
        Ok(())
    }

    pub fn signal_source_session_refresh(
        &self,
        socket_name: &str,
        session_name: &str,
    ) -> Result<(), LifecycleError> {
        if self.ensure_targeted_publication_owner(socket_name, session_name)? {
            self.signal_publication_owner_command(
                socket_name,
                session_name,
                PublicationOwnerCommand::Refresh,
            )?;
            return Ok(());
        }
        self.signal_publication_agent_command(
            socket_name,
            PublicationAgentCommand::PublishSession {
                session_name: session_name.to_string(),
            },
        )
    }

    pub fn signal_cached_source_session_refresh(
        &self,
        _socket_name: &str,
        _session_name: &str,
    ) -> Result<bool, LifecycleError> {
        Ok(false)
    }

    pub fn run_socket_lifecycle_hook(
        &self,
        command: SocketLifecycleHookCommand,
    ) -> Result<(), LifecycleError> {
        if !self.socket_is_live(&command.socket_name) {
            return Ok(());
        }
        self.ensure_publication_hooks_on_socket(&command.socket_name)?;
        let hook_name = command
            .hook_name
            .as_deref()
            .filter(|value| !value.is_empty());
        let session_name = command
            .session_name
            .as_deref()
            .filter(|value| !value.is_empty());

        match socket_lifecycle_publication_action(hook_name) {
            SocketLifecyclePublicationAction::TargetedPublish => {
                if let Some(session_name) = session_name {
                    if self.ensure_targeted_publication_owner(&command.socket_name, session_name)? {
                        let _ = self.signal_publication_owner_command(
                            &command.socket_name,
                            session_name,
                            PublicationOwnerCommand::Refresh,
                        );
                    }
                }
                Ok(())
            }
            SocketLifecyclePublicationAction::TargetedExit => {
                if let Some(session_name) = session_name {
                    if publication_owner_available(&remote_target_publication_owner_socket_path(
                        &command.socket_name,
                        session_name,
                    )) {
                        return Ok(());
                    }
                }
                self.ensure_configured_publications_on_socket(&command.socket_name)
            }
            SocketLifecyclePublicationAction::FullReconcile => {
                self.ensure_configured_publications_on_socket(&command.socket_name)
            }
        }
    }

    pub fn ensure_publication_server_running(
        &self,
        socket_name: &str,
    ) -> Result<(), LifecycleError> {
        let socket_path = remote_target_publication_socket_path(socket_name);
        if publication_server_available(&socket_path) {
            return Ok(());
        }
        if socket_path.exists() {
            let _ = fs::remove_file(&socket_path);
        }

        spawn_waitagent_sidecar(
            &self.current_executable,
            remote_target_publication_server_args(socket_name, &self.network),
        )
        .map_err(remote_target_publication_error)?;

        for _ in 0..PUBLICATION_SERVER_READY_RETRIES {
            if publication_server_available(&socket_path) {
                return Ok(());
            }
            thread::sleep(PUBLICATION_SERVER_READY_SLEEP);
        }

        Err(LifecycleError::Protocol(format!(
            "remote target publication server for socket `{socket_name}` did not become ready"
        )))
    }

    pub fn ensure_configured_publications_on_socket(
        &self,
        socket_name: &str,
    ) -> Result<(), LifecycleError> {
        let socket = TmuxSocketName::new(socket_name);
        if !self.local_tmux.socket_is_live(&socket) {
            return Ok(());
        }
        self.ensure_publication_hooks_on_socket(socket_name)?;
        let bindings = match list_publication_bindings_on_socket(&self.local_tmux, &socket) {
            Ok(bindings) => bindings,
            Err(error)
                if error.is_command_failure() && !self.local_tmux.socket_is_live(&socket) =>
            {
                return Ok(());
            }
            Err(error) => return Err(remote_target_publication_error(error)),
        };

        for binding in &bindings {
            self.ensure_publication_owner_running(socket_name, &binding.target_session_name)?;
        }
        Ok(())
    }

    fn ensure_publication_hooks_on_socket(&self, socket_name: &str) -> Result<(), LifecycleError> {
        let socket = TmuxSocketName::new(socket_name);
        if !self.local_tmux.socket_is_live(&socket) {
            return Ok(());
        }
        let hook_command = publication_socket_hook_tmux_command(
            self.current_executable.to_string_lossy().as_ref(),
            socket_name,
            &self.network,
        );
        for hook_name in PUBLICATION_GLOBAL_HOOKS {
            match self
                .local_tmux
                .set_global_hook_on_socket(socket_name, hook_name, &hook_command)
            {
                Ok(()) => {}
                Err(error)
                    if error.is_command_failure() && !self.local_tmux.socket_is_live(&socket) =>
                {
                    return Ok(());
                }
                Err(error) => return Err(remote_target_publication_error(error)),
            }
        }
        Ok(())
    }

    fn socket_is_live(&self, socket_name: &str) -> bool {
        self.local_tmux
            .socket_is_live(&TmuxSocketName::new(socket_name))
    }

    fn publish_bound_target_with_cache(
        &self,
        socket_name: &str,
        binding: &RemoteTargetPublicationBinding,
    ) -> Result<(), LifecycleError> {
        self.ensure_publication_server_running(socket_name)?;
        self.ensure_publication_sender_running(socket_name)?;
        let local_target = self
            .local_tmux
            .list_sessions_on_socket(&TmuxSocketName::new(socket_name))
            .map_err(remote_target_publication_error)?
            .into_iter()
            .find(|session| {
                session.address.session_id() == binding.target_session_name
                    && session.address.transport() == &SessionTransport::LocalTmux
                    && session.is_target_host()
            })
            .ok_or_else(|| {
                LifecycleError::Protocol(format!(
                    "target host session `{}` is not available on socket `{socket_name}` for remote publication",
                    binding.target_session_name
                ))
            })?;
        let published = published_remote_target_from_local(binding, &local_target);
        signal_publication_target_published(
            socket_name,
            &binding.authority_id,
            &published,
            Some(&binding.target_session_name),
        )
    }

    fn try_publish_bound_target_session_with_cache(
        &self,
        socket_name: &str,
        target_session_name: &str,
    ) -> Result<bool, LifecycleError> {
        let Some(binding) =
            self.find_remote_publication_binding_on_socket(socket_name, target_session_name)?
        else {
            return Ok(false);
        };
        self.publish_bound_target_with_cache(socket_name, &binding)?;
        Ok(true)
    }

    fn find_remote_publication_binding_on_socket(
        &self,
        socket_name: &str,
        target_session_name: &str,
    ) -> Result<Option<RemoteTargetPublicationBinding>, LifecycleError> {
        let bindings = list_publication_bindings_on_socket(
            &self.local_tmux,
            &TmuxSocketName::new(socket_name),
        )
        .map_err(remote_target_publication_error)?;
        Ok(bindings
            .into_iter()
            .find(|binding| binding.target_session_name == target_session_name))
    }

    fn signal_publication_reconcile(&self, socket_name: &str) -> Result<(), LifecycleError> {
        self.signal_publication_agent_command(socket_name, PublicationAgentCommand::FullReconcile)
    }

    fn signal_publication_agent_command(
        &self,
        socket_name: &str,
        command: PublicationAgentCommand,
    ) -> Result<(), LifecycleError> {
        self.ensure_publication_server_running(socket_name)?;
        self.ensure_publication_agent_running(socket_name)?;
        let mut stream =
            UnixStream::connect(remote_target_publication_agent_socket_path(socket_name))
                .map_err(remote_target_publication_error)?;
        stream
            .write_all(render_publication_agent_command(&command).as_bytes())
            .map_err(remote_target_publication_error)?;
        stream.flush().map_err(remote_target_publication_error)
    }

    fn ensure_publication_agent_running(&self, socket_name: &str) -> Result<(), LifecycleError> {
        let socket_path = remote_target_publication_agent_socket_path(socket_name);
        if publication_agent_available(&socket_path) {
            return Ok(());
        }
        if socket_path.exists() {
            let _ = fs::remove_file(&socket_path);
        }

        spawn_waitagent_sidecar(
            &self.current_executable,
            remote_target_publication_agent_args(socket_name, &self.network),
        )
        .map_err(remote_target_publication_error)?;

        for _ in 0..PUBLICATION_SERVER_READY_RETRIES {
            if publication_agent_available(&socket_path) {
                return Ok(());
            }
            thread::sleep(PUBLICATION_SERVER_READY_SLEEP);
        }

        Err(LifecycleError::Protocol(format!(
            "remote target publication agent for socket `{socket_name}` did not become ready"
        )))
    }

    pub(crate) fn ensure_publication_sender_running(
        &self,
        socket_name: &str,
    ) -> Result<(), LifecycleError> {
        ensure_publication_sender_process_running(
            &self.current_executable,
            socket_name,
            &self.network,
        )
    }

    fn ensure_publication_owner_running(
        &self,
        socket_name: &str,
        target_session_name: &str,
    ) -> Result<(), LifecycleError> {
        ensure_publication_owner_process_running(
            &self.current_executable,
            socket_name,
            target_session_name,
            &self.network,
        )
    }

    fn ensure_targeted_publication_owner(
        &self,
        socket_name: &str,
        target_session_name: &str,
    ) -> Result<bool, LifecycleError> {
        if self
            .find_remote_publication_binding_on_socket(socket_name, target_session_name)?
            .is_none()
        {
            return Ok(false);
        }
        self.ensure_publication_owner_running(socket_name, target_session_name)?;
        Ok(true)
    }

    fn signal_publication_owner_command(
        &self,
        socket_name: &str,
        target_session_name: &str,
        command: PublicationOwnerCommand,
    ) -> Result<(), LifecycleError> {
        signal_publication_owner_command(socket_name, target_session_name, command)
    }

    fn reconcile_socket_publications_with_cache(
        &self,
        socket_name: &str,
    ) -> Result<(), LifecycleError> {
        self.ensure_publication_server_running(socket_name)?;
        self.ensure_publication_sender_running(socket_name)?;
        let socket = TmuxSocketName::new(socket_name);
        let bindings = list_publication_bindings_on_socket(&self.local_tmux, &socket)
            .map_err(remote_target_publication_error)?;
        let local_targets = self
            .local_tmux
            .list_sessions_on_socket(&socket)
            .map_err(remote_target_publication_error)?;

        for binding in bindings {
            let Some(local_target) = local_targets.iter().find(|session| {
                session.address.session_id() == binding.target_session_name
                    && session.address.transport() == &SessionTransport::LocalTmux
                    && session.is_target_host()
            }) else {
                continue;
            };
            let published = published_remote_target_from_local(&binding, local_target);
            signal_publication_target_published(
                socket_name,
                &binding.authority_id,
                &published,
                Some(&binding.target_session_name),
            )?;
        }
        Ok(())
    }

    fn process_publication_agent_command(
        &self,
        socket_name: &str,
        command: PublicationAgentCommand,
    ) -> Result<(), LifecycleError> {
        match command {
            PublicationAgentCommand::FullReconcile => {
                self.reconcile_socket_publications_with_cache(socket_name)
            }
            PublicationAgentCommand::PublishSession { session_name } => self
                .try_publish_bound_target_session_with_cache(socket_name, &session_name)
                .map(|_| ()),
            PublicationAgentCommand::ExitTarget {
                authority_id,
                transport_session_id,
                source_session_name,
            } => {
                self.ensure_publication_sender_running(socket_name)?;
                signal_publication_target_exited(
                    socket_name,
                    &authority_id,
                    &transport_session_id,
                    source_session_name.as_deref(),
                )
            }
        }
    }
}

#[cfg(test)]
mod remote_target_publication_runtime_test;
