use crate::cli::{
    RemoteNetworkConfig, RemoteTargetBindPublicationCommand, RemoteTargetPublicationAgentCommand,
    RemoteTargetPublicationOwnerCommand, RemoteTargetPublicationServerCommand,
    RemoteTargetReconcilePublicationsCommand, RemoteTargetUnbindPublicationCommand,
    SocketLifecycleHookCommand,
};
use crate::domain::session_catalog::{ManagedSessionRecord, SessionTransport};
use crate::infra::published_target_store::{PublishedTargetSourceBinding, PublishedTargetStore};
use crate::infra::remote_protocol::{ControlPlanePayload, NodeSessionChannel, ProtocolEnvelope};
use crate::infra::remote_transport_codec::read_node_session_envelope;
use crate::infra::tmux::{
    EmbeddedTmuxBackend, RemoteTargetPublicationBinding, TmuxSessionGateway, TmuxSocketName,
};
use crate::lifecycle::LifecycleError;
use crate::runtime::remote_node_transport_runtime::{read_client_hello, write_server_hello};
use crate::runtime::remote_runtime_owner_runtime::RemoteRuntimeOwnerRuntime;
use crate::runtime::remote_target_publication_transport_runtime::remote_target_publication_socket_path;
use crate::runtime::sidecar_process_runtime::spawn_waitagent_sidecar;
use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::thread;

mod publication_helpers;
pub(crate) use publication_helpers::*;

#[derive(Clone)]
pub struct RemoteTargetPublicationRuntime {
    store: PublishedTargetStore,
    remote_runtime_owner: RemoteRuntimeOwnerRuntime,
    local_tmux: EmbeddedTmuxBackend,
    current_executable: PathBuf,
    network: RemoteNetworkConfig,
}

impl RemoteTargetPublicationRuntime {
    pub fn from_build_env() -> Result<Self, LifecycleError> {
        Self::from_build_env_with_network(RemoteNetworkConfig::default())
    }

    pub fn from_build_env_with_network(
        network: RemoteNetworkConfig,
    ) -> Result<Self, LifecycleError> {
        Ok(Self {
            store: PublishedTargetStore::default(),
            remote_runtime_owner: RemoteRuntimeOwnerRuntime::from_build_env_with_network(
                network.clone(),
            )?,
            local_tmux: EmbeddedTmuxBackend::from_build_env()
                .map_err(remote_target_publication_error)?,
            current_executable: std::env::current_exe().map_err(|error| {
                LifecycleError::Io(
                    "failed to locate current waitagent executable".to_string(),
                    error,
                )
            })?,
            network,
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
            let store = self.store.clone();
            let source_socket_name = command.socket_name.clone();
            let current_executable = self.current_executable.clone();
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
                    let changed = match apply_publication_envelope(
                        &store,
                        &source_socket_name,
                        &session_envelope.envelope,
                    ) {
                        Ok(changed) => changed,
                        Err(_) => break,
                    };
                    if changed
                        && spawn_socket_chrome_refresh(&current_executable, &source_socket_name)
                            .is_err()
                    {
                        break;
                    }
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
        let changed = apply_publication_envelope(&self.store, socket_name, &envelope)?;
        if changed {
            spawn_socket_chrome_refresh(&self.current_executable, socket_name)?;
        }
        Ok(())
    }

    pub fn apply_discovered_remote_session_envelope_on_socket(
        &self,
        socket_name: &str,
        node_id: &str,
        envelope: ProtocolEnvelope<ControlPlanePayload>,
    ) -> Result<(), LifecycleError> {
        let live_workspace_sockets = vec![socket_name.to_string()];
        let remote_session = discovered_remote_session_from_envelope(node_id, &envelope)?;
        if let Some(session) = remote_session.published_session {
            if is_publishable_discovered_remote_session(&session) {
                self.signal_remote_runtime_owner_upsert_live_workspaces(
                    &live_workspace_sockets,
                    node_id,
                    &session,
                )?;
            }
        }
        if let Some((authority_id, transport_session_id)) = remote_session.exited_session {
            self.signal_remote_runtime_owner_remove_live_workspaces(
                &live_workspace_sockets,
                node_id,
                &authority_id,
                &transport_session_id,
            )?;
        }
        if !live_workspace_sockets.is_empty() {
            self.refresh_live_workspaces(&live_workspace_sockets)?;
        }
        Ok(())
    }

    pub fn remove_discovered_remote_node_on_socket(
        &self,
        socket_name: &str,
        node_id: &str,
    ) -> Result<(), LifecycleError> {
        let live_workspace_sockets = vec![socket_name.to_string()];
        self.signal_remote_runtime_owner_remove_node_live_workspaces(
            &live_workspace_sockets,
            node_id,
        )?;
        if !live_workspace_sockets.is_empty() {
            self.refresh_live_workspaces(&live_workspace_sockets)?;
        }
        Ok(())
    }

    pub(crate) fn apply_discovered_remote_session_envelope_on_live_workspaces(
        &self,
        node_id: &str,
        envelope: ProtocolEnvelope<ControlPlanePayload>,
    ) -> Result<(), LifecycleError> {
        let live_workspace_sockets = self.live_workspace_socket_names()?;
        for socket_name in &live_workspace_sockets {
            self.apply_discovered_remote_session_envelope_on_socket(
                socket_name,
                node_id,
                envelope.clone(),
            )?;
        }
        Ok(())
    }

    pub(crate) fn remove_discovered_remote_node_on_live_workspaces(
        &self,
        node_id: &str,
    ) -> Result<(), LifecycleError> {
        let live_workspace_sockets = self.live_workspace_socket_names()?;
        for socket_name in &live_workspace_sockets {
            self.remove_discovered_remote_node_on_socket(socket_name, node_id)?;
        }
        Ok(())
    }

    pub fn mark_source_target_offline(
        &self,
        socket_name: &str,
        session_name: &str,
        target_id: &str,
    ) -> Result<(), LifecycleError> {
        let changed =
            mark_target_offline_in_store(&self.store, socket_name, session_name, target_id)?;
        if changed {
            spawn_socket_chrome_refresh(&self.current_executable, socket_name)?;
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

    fn signal_remote_runtime_owner_upsert_live_workspaces(
        &self,
        socket_names: &[String],
        node_id: &str,
        session: &ManagedSessionRecord,
    ) -> Result<(), LifecycleError> {
        for socket_name in socket_names {
            self.remote_runtime_owner
                .upsert_session(socket_name, node_id, session)?;
        }
        Ok(())
    }

    fn signal_remote_runtime_owner_remove_live_workspaces(
        &self,
        socket_names: &[String],
        node_id: &str,
        authority_id: &str,
        transport_session_id: &str,
    ) -> Result<(), LifecycleError> {
        for socket_name in socket_names {
            self.remote_runtime_owner.remove_session(
                socket_name,
                node_id,
                authority_id,
                transport_session_id,
            )?;
        }
        Ok(())
    }

    fn signal_remote_runtime_owner_remove_node_live_workspaces(
        &self,
        socket_names: &[String],
        node_id: &str,
    ) -> Result<(), LifecycleError> {
        for socket_name in socket_names {
            self.remote_runtime_owner
                .remove_node(socket_name, node_id)?;
        }
        Ok(())
    }

    pub(crate) fn live_workspace_socket_names(&self) -> Result<Vec<String>, LifecycleError> {
        let mut all_sessions = self
            .local_tmux
            .list_sessions()
            .map_err(remote_target_publication_error)?;
        if let Ok(managed_sockets) = self.local_tmux.discover_waitagent_sockets() {
            for socket in &managed_sockets {
                if let Ok(sessions) = self
                    .local_tmux
                    .list_sessions_on_socket(socket)
                    .map_err(remote_target_publication_error)
                {
                    all_sessions.extend(sessions);
                }
            }
        }
        Ok(live_workspace_socket_names_from_sessions(&all_sessions))
    }

    fn refresh_live_workspaces(&self, socket_names: &[String]) -> Result<(), LifecycleError> {
        for socket_name in socket_names {
            spawn_socket_chrome_refresh(&self.current_executable, socket_name)?;
        }
        Ok(())
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
        if self
            .signal_publication_owner_command(
                socket_name,
                session_name,
                PublicationOwnerCommand::Stop,
            )
            .is_ok()
        {
            return Ok(());
        }
        let records = self
            .store
            .list_records_for_source_binding(socket_name, session_name)
            .map_err(remote_target_publication_error)?;
        for record in records {
            self.signal_publication_agent_command(
                socket_name,
                PublicationAgentCommand::ExitTarget {
                    authority_id: record.target.address.authority_id().to_string(),
                    transport_session_id: record.target.address.session_id().to_string(),
                    source_session_name: Some(session_name.to_string()),
                },
            )?;
        }
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
        socket_name: &str,
        session_name: &str,
    ) -> Result<bool, LifecycleError> {
        let records = self
            .store
            .list_records_for_source_binding(socket_name, session_name)
            .map_err(remote_target_publication_error)?;
        if records.is_empty() {
            return Ok(false);
        }
        for record in records {
            signal_publication_target_published(
                socket_name,
                record.target.address.authority_id(),
                &record.target,
                Some(session_name),
            )?;
        }
        Ok(true)
    }

    pub fn run_socket_lifecycle_hook(
        &self,
        command: SocketLifecycleHookCommand,
    ) -> Result<(), LifecycleError> {
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
                    let records = self
                        .store
                        .list_records_for_source_binding(&command.socket_name, session_name)
                        .map_err(remote_target_publication_error)?;
                    if records.is_empty() {
                        return self.ensure_configured_publications_on_socket(&command.socket_name);
                    }
                    for record in records {
                        self.signal_publication_agent_command(
                            &command.socket_name,
                            PublicationAgentCommand::ExitTarget {
                                authority_id: record.target.address.authority_id().to_string(),
                                transport_session_id: record
                                    .target
                                    .address
                                    .session_id()
                                    .to_string(),
                                source_session_name: Some(session_name.to_string()),
                            },
                        )?;
                    }
                    return Ok(());
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
        self.ensure_publication_hooks_on_socket(socket_name)?;
        let socket = TmuxSocketName::new(socket_name);
        let bindings = list_publication_bindings_on_socket(&self.local_tmux, &socket)
            .map_err(remote_target_publication_error)?;
        let has_published_records = !self
            .store
            .list_records_for_source_socket(socket_name)
            .map_err(remote_target_publication_error)?
            .is_empty();

        for binding in &bindings {
            self.ensure_publication_owner_running(socket_name, &binding.target_session_name)?;
        }
        if has_published_records {
            self.signal_publication_reconcile(socket_name)?;
        }
        Ok(())
    }

    fn ensure_publication_hooks_on_socket(&self, socket_name: &str) -> Result<(), LifecycleError> {
        let hook_command = publication_socket_hook_tmux_command(
            self.current_executable.to_string_lossy().as_ref(),
            socket_name,
        );
        for hook_name in PUBLICATION_GLOBAL_HOOKS {
            self.local_tmux
                .set_global_hook_on_socket(socket_name, hook_name, &hook_command)
                .map_err(remote_target_publication_error)?;
        }
        Ok(())
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
        let mut keep_source_bindings = BTreeSet::new();

        for binding in bindings {
            let Some(local_target) = local_targets.iter().find(|session| {
                session.address.session_id() == binding.target_session_name
                    && session.address.transport() == &SessionTransport::LocalTmux
                    && session.is_target_host()
            }) else {
                continue;
            };
            let published = published_remote_target_from_local(&binding, local_target);
            keep_source_bindings.insert(PublishedTargetSourceBinding {
                socket_name: socket_name.to_string(),
                session_name: Some(binding.target_session_name.clone()),
            });
            signal_publication_target_published(
                socket_name,
                &binding.authority_id,
                &published,
                Some(&binding.target_session_name),
            )?;
        }

        let stale = self
            .store
            .list_records_for_source_socket(socket_name)
            .map_err(remote_target_publication_error)?;
        for record in stale {
            for source_binding in record.source_bindings.iter().filter(|binding| {
                binding.socket_name == socket_name && !keep_source_bindings.contains(binding)
            }) {
                signal_publication_target_exited(
                    socket_name,
                    record.target.address.authority_id(),
                    record.target.address.session_id(),
                    source_binding.session_name.as_deref(),
                )?;
            }
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
