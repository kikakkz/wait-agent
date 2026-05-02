use crate::cli::{
    prepend_global_network_args, RemoteNetworkConfig, RemoteTargetBindPublicationCommand,
    RemoteTargetPublicationAgentCommand, RemoteTargetPublicationOwnerCommand,
    RemoteTargetPublicationServerCommand, RemoteTargetReconcilePublicationsCommand,
    RemoteTargetUnbindPublicationCommand, SocketLifecycleHookCommand,
};
use crate::domain::session_catalog::{
    ManagedSessionAddress, ManagedSessionRecord, ManagedSessionTaskState, SessionAvailability,
    SessionTransport,
};
use crate::domain::workspace::WorkspaceSessionRole;
use crate::infra::base64::{decode_base64, encode_base64};
use crate::infra::discovered_remote_session_store::DiscoveredRemoteSessionStore;
use crate::infra::published_target_store::{PublishedTargetSourceBinding, PublishedTargetStore};
use crate::infra::remote_protocol::{
    ControlPlanePayload, NodeSessionChannel, ProtocolEnvelope, TargetPublishedPayload,
};
use crate::infra::remote_transport_codec::read_node_session_envelope;
use crate::infra::tmux::{
    EmbeddedTmuxBackend, RemoteTargetPublicationBinding, TmuxSessionGateway, TmuxSocketName,
};
use crate::lifecycle::LifecycleError;
use crate::runtime::remote_node_transport_runtime::{read_client_hello, write_server_hello};
use crate::runtime::remote_runtime_owner_runtime::RemoteRuntimeOwnerRuntime;
use crate::runtime::remote_target_publication_transport_runtime::remote_target_publication_socket_path;
use std::collections::BTreeSet;
use std::fs;
use std::io::{self, ErrorKind, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::str;
use std::thread;
use std::time::Duration;

const PUBLICATION_SERVER_READY_RETRIES: usize = 20;
const PUBLICATION_SERVER_READY_SLEEP: Duration = Duration::from_millis(25);
const PUBLICATION_OWNER_POLL_INTERVAL: Duration = Duration::from_millis(500);
const PUBLICATION_GLOBAL_HOOKS: [&str; 4] = [
    "session-created",
    "session-closed",
    "client-attached",
    "client-detached",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SocketLifecyclePublicationAction {
    TargetedPublish,
    TargetedExit,
    FullReconcile,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PublicationAgentCommand {
    FullReconcile,
    PublishSession {
        session_name: String,
    },
    ExitTarget {
        authority_id: String,
        transport_session_id: String,
        source_session_name: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PublicationSenderCommand {
    RegisterLiveSession {
        target_session_name: String,
        authority_id: String,
        target_id: String,
        transport_socket_path: String,
    },
    UnregisterLiveSession {
        target_session_name: String,
    },
    PublishTarget {
        authority_id: String,
        transport_session_id: String,
        source_session_name: Option<String>,
        selector: Option<String>,
        availability: &'static str,
        session_role: Option<&'static str>,
        workspace_key: Option<String>,
        command_name: Option<String>,
        current_path: Option<String>,
        attached_clients: usize,
        window_count: usize,
        task_state: &'static str,
    },
    ExitTarget {
        authority_id: String,
        transport_session_id: String,
        source_session_name: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PublicationOwnerCommand {
    Refresh,
    Stop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct PublicationOwnerDrain {
    refresh_requested: bool,
    stop_requested: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PublicationOwnerSnapshot {
    authority_id: String,
    transport_session_id: String,
    selector: Option<String>,
    availability: SessionAvailability,
    workspace_key: Option<String>,
    session_role: Option<WorkspaceSessionRole>,
    attached_clients: usize,
    window_count: usize,
    command_name: Option<String>,
    current_path: Option<PathBuf>,
}

#[derive(Clone)]
pub struct RemoteTargetPublicationRuntime {
    store: PublishedTargetStore,
    discovered_store: DiscoveredRemoteSessionStore,
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
            discovered_store: DiscoveredRemoteSessionStore::default(),
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

    pub fn apply_discovered_remote_session_envelope(
        &self,
        node_id: &str,
        envelope: ProtocolEnvelope<ControlPlanePayload>,
    ) -> Result<(), LifecycleError> {
        let remote_session = discovered_remote_session_from_envelope(node_id, &envelope)?;
        let changed =
            apply_discovered_remote_session_envelope(&self.discovered_store, node_id, &envelope)?;
        let mut owner_changed = false;
        if let Some(session) = remote_session.published_session {
            owner_changed |=
                signal_remote_runtime_owner_upsert_all(&self.remote_runtime_owner, node_id, &session)?;
        }
        if let Some((authority_id, transport_session_id)) = remote_session.exited_session {
            owner_changed |= signal_remote_runtime_owner_remove_all(
                &self.remote_runtime_owner,
                node_id,
                &authority_id,
                &transport_session_id,
            )?;
        }
        if should_refresh_discovered_remote_catalog(changed, owner_changed) {
            spawn_chrome_refresh_all(&self.current_executable)?;
        }
        Ok(())
    }

    pub fn mark_discovered_remote_node_offline(&self, node_id: &str) -> Result<(), LifecycleError> {
        let changed =
            mark_discovered_remote_node_offline_in_store(&self.discovered_store, node_id)?;
        let owner_changed =
            signal_remote_runtime_owner_mark_offline_all(&self.remote_runtime_owner, node_id)?;
        if should_refresh_discovered_remote_catalog(changed, owner_changed) {
            spawn_chrome_refresh_all(&self.current_executable)?;
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
        self.local_tmux
            .bind_remote_publication_on_socket(
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
        self.local_tmux
            .unbind_remote_publication_on_socket(&command.socket_name, &command.target_session_name)
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

        Command::new(&self.current_executable)
            .args(remote_target_publication_server_args(
                socket_name,
                &self.network,
            ))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
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
        let bindings = self
            .local_tmux
            .list_remote_publication_bindings_on_socket(&socket)
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
        let bindings = self
            .local_tmux
            .list_remote_publication_bindings_on_socket(&TmuxSocketName::new(socket_name))
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

        Command::new(&self.current_executable)
            .args(remote_target_publication_agent_args(
                socket_name,
                &self.network,
            ))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
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
        let bindings = self
            .local_tmux
            .list_remote_publication_bindings_on_socket(&socket)
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

pub(crate) fn ensure_publication_owner_process_running(
    current_executable: &std::path::Path,
    socket_name: &str,
    target_session_name: &str,
    network: &RemoteNetworkConfig,
) -> Result<(), LifecycleError> {
    let socket_path = remote_target_publication_owner_socket_path(socket_name, target_session_name);
    if publication_owner_available(&socket_path) {
        return Ok(());
    }
    if socket_path.exists() {
        let _ = fs::remove_file(&socket_path);
    }

    Command::new(current_executable)
        .args(remote_target_publication_owner_args(
            socket_name,
            target_session_name,
            network,
        ))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(remote_target_publication_error)?;

    for _ in 0..PUBLICATION_SERVER_READY_RETRIES {
        if publication_owner_available(&socket_path) {
            return Ok(());
        }
        thread::sleep(PUBLICATION_SERVER_READY_SLEEP);
    }

    Err(LifecycleError::Protocol(format!(
        "remote target publication owner for socket `{socket_name}` session `{target_session_name}` did not become ready"
    )))
}

pub(crate) fn ensure_publication_sender_process_running(
    current_executable: &std::path::Path,
    socket_name: &str,
    network: &RemoteNetworkConfig,
) -> Result<(), LifecycleError> {
    let socket_path = remote_target_publication_sender_socket_path(socket_name);
    if publication_sender_available(&socket_path) {
        return Ok(());
    }
    if socket_path.exists() {
        let _ = fs::remove_file(&socket_path);
    }

    Command::new(current_executable)
        .args(remote_target_publication_sender_args(socket_name, network))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(remote_target_publication_error)?;

    for _ in 0..PUBLICATION_SERVER_READY_RETRIES {
        if publication_sender_available(&socket_path) {
            return Ok(());
        }
        thread::sleep(PUBLICATION_SERVER_READY_SLEEP);
    }

    Err(LifecycleError::Protocol(format!(
        "remote target publication sender for socket `{socket_name}` did not become ready"
    )))
}

pub(crate) fn signal_publication_sender_live_session_registered(
    socket_name: &str,
    target_session_name: &str,
    authority_id: &str,
    target_id: &str,
    transport_socket_path: &str,
) -> Result<(), LifecycleError> {
    signal_publication_sender_command(
        socket_name,
        PublicationSenderCommand::RegisterLiveSession {
            target_session_name: target_session_name.to_string(),
            authority_id: authority_id.to_string(),
            target_id: target_id.to_string(),
            transport_socket_path: transport_socket_path.to_string(),
        },
    )
}

pub(crate) fn signal_publication_sender_live_session_unregistered(
    socket_name: &str,
    target_session_name: &str,
) -> Result<(), LifecycleError> {
    signal_publication_sender_command(
        socket_name,
        PublicationSenderCommand::UnregisterLiveSession {
            target_session_name: target_session_name.to_string(),
        },
    )
}

pub(crate) fn signal_publication_target_published(
    socket_name: &str,
    authority_id: &str,
    target: &ManagedSessionRecord,
    source_session_name: Option<&str>,
) -> Result<(), LifecycleError> {
    signal_publication_sender_command(
        socket_name,
        PublicationSenderCommand::PublishTarget {
            authority_id: authority_id.to_string(),
            transport_session_id: target.address.session_id().to_string(),
            source_session_name: source_session_name.map(str::to_string),
            selector: target.selector.clone(),
            availability: target.availability.as_str(),
            session_role: target.session_role.map(|role| role.as_str()),
            workspace_key: target.workspace_key.clone(),
            command_name: target.command_name.clone(),
            current_path: target
                .current_path
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned()),
            attached_clients: target.attached_clients,
            window_count: target.window_count,
            task_state: target.task_state.as_str(),
        },
    )
}

pub(crate) fn signal_publication_target_exited(
    socket_name: &str,
    authority_id: &str,
    transport_session_id: &str,
    source_session_name: Option<&str>,
) -> Result<(), LifecycleError> {
    signal_publication_sender_command(
        socket_name,
        PublicationSenderCommand::ExitTarget {
            authority_id: authority_id.to_string(),
            transport_session_id: transport_session_id.to_string(),
            source_session_name: source_session_name.map(str::to_string),
        },
    )
}

pub(crate) fn signal_publication_sender_command(
    socket_name: &str,
    command: PublicationSenderCommand,
) -> Result<(), LifecycleError> {
    let mut stream = UnixStream::connect(remote_target_publication_sender_socket_path(socket_name))
        .map_err(remote_target_publication_error)?;
    stream
        .write_all(render_publication_sender_command(&command).as_bytes())
        .map_err(remote_target_publication_error)?;
    stream.flush().map_err(remote_target_publication_error)
}

fn published_remote_target_record_from_payload(
    authority_id: &str,
    payload: &TargetPublishedPayload,
) -> Result<ManagedSessionRecord, LifecycleError> {
    let availability = SessionAvailability::parse(payload.availability).ok_or_else(|| {
        LifecycleError::Protocol(format!(
            "unsupported remote target availability `{}`",
            payload.availability
        ))
    })?;
    let session_role = payload
        .session_role
        .map(|value| {
            WorkspaceSessionRole::parse(value).ok_or_else(|| {
                LifecycleError::Protocol(format!(
                    "unsupported remote target session role `{value}`"
                ))
            })
        })
        .transpose()?;

    Ok(ManagedSessionRecord {
        address: ManagedSessionAddress::remote_peer(
            authority_id,
            payload.transport_session_id.clone(),
        ),
        selector: payload.selector.clone(),
        availability,
        workspace_dir: None,
        workspace_key: payload.workspace_key.clone(),
        session_role,
        opened_by: Vec::new(),
        attached_clients: payload.attached_clients,
        window_count: payload.window_count,
        command_name: payload.command_name.clone(),
        current_path: payload.current_path.as_ref().map(PathBuf::from),
        task_state: ManagedSessionTaskState::parse(payload.task_state).ok_or_else(|| {
            LifecycleError::Protocol(format!(
                "unsupported remote target task state `{}`",
                payload.task_state
            ))
        })?,
    })
}

fn remote_target_publication_error<E>(error: E) -> LifecycleError
where
    E: ToString,
{
    LifecycleError::Io(
        "failed to update published remote target catalog".to_string(),
        io::Error::new(io::ErrorKind::Other, error.to_string()),
    )
}

fn remote_target_publication_server_args(
    socket_name: &str,
    network: &RemoteNetworkConfig,
) -> Vec<String> {
    prepend_global_network_args(
        vec![
            "__remote-target-publication-server".to_string(),
            "--socket-name".to_string(),
            socket_name.to_string(),
        ],
        network,
    )
}

fn remote_target_publication_agent_args(
    socket_name: &str,
    network: &RemoteNetworkConfig,
) -> Vec<String> {
    prepend_global_network_args(
        vec![
            "__remote-target-publication-agent".to_string(),
            "--socket-name".to_string(),
            socket_name.to_string(),
        ],
        network,
    )
}

pub(crate) fn remote_target_publication_sender_args(
    socket_name: &str,
    network: &RemoteNetworkConfig,
) -> Vec<String> {
    prepend_global_network_args(
        vec![
            "__remote-target-publication-sender".to_string(),
            "--socket-name".to_string(),
            socket_name.to_string(),
        ],
        network,
    )
}

fn remote_target_publication_owner_args(
    socket_name: &str,
    target_session_name: &str,
    network: &RemoteNetworkConfig,
) -> Vec<String> {
    prepend_global_network_args(
        vec![
            "__remote-target-publication-owner".to_string(),
            "--socket-name".to_string(),
            socket_name.to_string(),
            "--target-session-name".to_string(),
            target_session_name.to_string(),
        ],
        network,
    )
}

fn publication_socket_hook_tmux_command(executable: &str, socket_name: &str) -> String {
    let hook_command = [
        shell_escape(executable),
        shell_escape("__socket-lifecycle-hook"),
        shell_escape("--socket-name"),
        shell_escape(socket_name),
        shell_escape("--hook-name"),
        shell_escape("#{hook}"),
        shell_escape("--session-name"),
        shell_escape("#{hook_session_name}"),
    ]
    .join(" ");
    format!(
        "run-shell -b {}",
        tmux_quote_argument(&format!("{hook_command} >/dev/null 2>&1"))
    )
}

fn shell_escape(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn tmux_quote_argument(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn socket_lifecycle_publication_action(
    hook_name: Option<&str>,
) -> SocketLifecyclePublicationAction {
    match hook_name {
        Some("client-attached") | Some("client-detached") | Some("session-created") => {
            SocketLifecyclePublicationAction::TargetedPublish
        }
        Some("session-closed") => SocketLifecyclePublicationAction::TargetedExit,
        Some(_) | None => SocketLifecyclePublicationAction::FullReconcile,
    }
}

fn publication_server_available(socket_path: &std::path::Path) -> bool {
    std::os::unix::net::UnixStream::connect(socket_path).is_ok()
}

fn publication_agent_available(socket_path: &std::path::Path) -> bool {
    std::os::unix::net::UnixStream::connect(socket_path).is_ok()
}

fn publication_sender_available(socket_path: &std::path::Path) -> bool {
    std::os::unix::net::UnixStream::connect(socket_path).is_ok()
}

fn publication_owner_available(socket_path: &std::path::Path) -> bool {
    std::os::unix::net::UnixStream::connect(socket_path).is_ok()
}

fn remote_target_publication_agent_socket_path(socket_name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "waitagent-remote-publication-agent-{}.sock",
        sanitize_path_component(socket_name)
    ))
}

fn remote_target_publication_owner_socket_path(
    socket_name: &str,
    target_session_name: &str,
) -> PathBuf {
    std::env::temp_dir().join(format!(
        "waitagent-remote-publication-owner-{}-{}.sock",
        sanitize_path_component(socket_name),
        sanitize_path_component(target_session_name)
    ))
}

pub(crate) fn remote_target_publication_sender_socket_path(socket_name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "waitagent-remote-publication-sender-{}.sock",
        sanitize_path_component(socket_name)
    ))
}

fn render_publication_agent_command(command: &PublicationAgentCommand) -> String {
    match command {
        PublicationAgentCommand::FullReconcile => "full_reconcile\n".to_string(),
        PublicationAgentCommand::PublishSession { session_name } => format!(
            "publish_session\t{}\n",
            encode_base64(session_name.as_bytes())
        ),
        PublicationAgentCommand::ExitTarget {
            authority_id,
            transport_session_id,
            source_session_name,
        } => format!(
            "exit_target\t{}\t{}\t{}\n",
            encode_base64(authority_id.as_bytes()),
            encode_base64(transport_session_id.as_bytes()),
            encode_optional_agent_field(source_session_name.as_deref())
        ),
    }
}

pub(crate) fn render_publication_sender_command(command: &PublicationSenderCommand) -> String {
    match command {
        PublicationSenderCommand::RegisterLiveSession {
            target_session_name,
            authority_id,
            target_id,
            transport_socket_path,
        } => format!(
            "register_live_session\t{}\t{}\t{}\t{}\n",
            encode_base64(target_session_name.as_bytes()),
            encode_base64(authority_id.as_bytes()),
            encode_base64(target_id.as_bytes()),
            encode_base64(transport_socket_path.as_bytes())
        ),
        PublicationSenderCommand::UnregisterLiveSession {
            target_session_name,
        } => format!(
            "unregister_live_session\t{}\n",
            encode_base64(target_session_name.as_bytes())
        ),
        PublicationSenderCommand::PublishTarget {
            authority_id,
            transport_session_id,
            source_session_name,
            selector,
            availability,
            session_role,
            workspace_key,
            command_name,
            current_path,
            attached_clients,
            window_count,
            task_state,
        } => format!(
            "publish_target\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
            encode_base64(authority_id.as_bytes()),
            encode_base64(transport_session_id.as_bytes()),
            encode_optional_agent_field(source_session_name.as_deref()),
            encode_optional_agent_field(selector.as_deref()),
            availability,
            encode_optional_static_agent_field(*session_role),
            encode_optional_agent_field(workspace_key.as_deref()),
            encode_optional_agent_field(command_name.as_deref()),
            encode_optional_agent_field(current_path.as_deref()),
            attached_clients,
            window_count,
            task_state,
        ),
        PublicationSenderCommand::ExitTarget {
            authority_id,
            transport_session_id,
            source_session_name,
        } => format!(
            "exit_target\t{}\t{}\t{}\n",
            encode_base64(authority_id.as_bytes()),
            encode_base64(transport_session_id.as_bytes()),
            encode_optional_agent_field(source_session_name.as_deref())
        ),
    }
}

fn render_publication_owner_command(command: PublicationOwnerCommand) -> &'static str {
    match command {
        PublicationOwnerCommand::Refresh => "refresh\n",
        PublicationOwnerCommand::Stop => "stop\n",
    }
}

fn signal_publication_owner_command(
    socket_name: &str,
    target_session_name: &str,
    command: PublicationOwnerCommand,
) -> Result<(), LifecycleError> {
    let mut stream = UnixStream::connect(remote_target_publication_owner_socket_path(
        socket_name,
        target_session_name,
    ))
    .map_err(remote_target_publication_error)?;
    stream
        .write_all(render_publication_owner_command(command).as_bytes())
        .map_err(remote_target_publication_error)?;
    stream.flush().map_err(remote_target_publication_error)
}

fn read_publication_agent_command(
    reader: &mut impl Read,
) -> Result<PublicationAgentCommand, LifecycleError> {
    let mut bytes = Vec::new();
    reader
        .read_to_end(&mut bytes)
        .map_err(remote_target_publication_error)?;
    let line = str::from_utf8(&bytes)
        .map_err(remote_target_publication_error)?
        .trim();
    parse_publication_agent_command(line)
}

pub(crate) fn read_publication_sender_command(
    reader: &mut impl Read,
) -> Result<PublicationSenderCommand, LifecycleError> {
    let mut bytes = Vec::new();
    reader
        .read_to_end(&mut bytes)
        .map_err(remote_target_publication_error)?;
    let line = str::from_utf8(&bytes)
        .map_err(remote_target_publication_error)?
        .trim();
    parse_publication_sender_command(line)
}

fn parse_publication_agent_command(line: &str) -> Result<PublicationAgentCommand, LifecycleError> {
    let mut parts = line.split('\t');
    match parts.next().unwrap_or_default() {
        "full_reconcile" => Ok(PublicationAgentCommand::FullReconcile),
        "publish_session" => {
            let session_name =
                decode_publication_agent_string_field(parts.next().ok_or_else(|| {
                    LifecycleError::Protocol("publish_session is missing session field".to_string())
                })?)?;
            if parts.next().is_some() {
                return Err(LifecycleError::Protocol(
                    "publish_session contains unexpected extra fields".to_string(),
                ));
            }
            Ok(PublicationAgentCommand::PublishSession { session_name })
        }
        "exit_target" => {
            let authority_id =
                decode_publication_agent_string_field(parts.next().ok_or_else(|| {
                    LifecycleError::Protocol("exit_target is missing authority field".to_string())
                })?)?;
            let transport_session_id =
                decode_publication_agent_string_field(parts.next().ok_or_else(|| {
                    LifecycleError::Protocol("exit_target is missing session field".to_string())
                })?)?;
            let source_session_name =
                decode_optional_agent_field(parts.next().ok_or_else(|| {
                    LifecycleError::Protocol(
                        "exit_target is missing source session field".to_string(),
                    )
                })?)?;
            if parts.next().is_some() {
                return Err(LifecycleError::Protocol(
                    "exit_target contains unexpected extra fields".to_string(),
                ));
            }
            Ok(PublicationAgentCommand::ExitTarget {
                authority_id,
                transport_session_id,
                source_session_name,
            })
        }
        other => Err(LifecycleError::Protocol(format!(
            "unsupported remote publication agent command `{other}`"
        ))),
    }
}

fn parse_publication_sender_command(
    line: &str,
) -> Result<PublicationSenderCommand, LifecycleError> {
    let mut parts = line.split('\t');
    match parts.next().unwrap_or_default() {
        "register_live_session" => {
            let target_session_name =
                decode_publication_agent_string_field(parts.next().ok_or_else(|| {
                    LifecycleError::Protocol(
                        "register_live_session is missing target session field".to_string(),
                    )
                })?)?;
            let authority_id =
                decode_publication_agent_string_field(parts.next().ok_or_else(|| {
                    LifecycleError::Protocol(
                        "register_live_session is missing authority field".to_string(),
                    )
                })?)?;
            let target_id =
                decode_publication_agent_string_field(parts.next().ok_or_else(|| {
                    LifecycleError::Protocol(
                        "register_live_session is missing target id field".to_string(),
                    )
                })?)?;
            let transport_socket_path =
                decode_publication_agent_string_field(parts.next().ok_or_else(|| {
                    LifecycleError::Protocol(
                        "register_live_session is missing transport socket field".to_string(),
                    )
                })?)?;
            if parts.next().is_some() {
                return Err(LifecycleError::Protocol(
                    "register_live_session contains unexpected extra fields".to_string(),
                ));
            }
            Ok(PublicationSenderCommand::RegisterLiveSession {
                target_session_name,
                authority_id,
                target_id,
                transport_socket_path,
            })
        }
        "unregister_live_session" => {
            let target_session_name =
                decode_publication_agent_string_field(parts.next().ok_or_else(|| {
                    LifecycleError::Protocol(
                        "unregister_live_session is missing target session field".to_string(),
                    )
                })?)?;
            if parts.next().is_some() {
                return Err(LifecycleError::Protocol(
                    "unregister_live_session contains unexpected extra fields".to_string(),
                ));
            }
            Ok(PublicationSenderCommand::UnregisterLiveSession {
                target_session_name,
            })
        }
        "publish_target" => {
            let authority_id =
                decode_publication_agent_string_field(parts.next().ok_or_else(|| {
                    LifecycleError::Protocol(
                        "publish_target is missing authority field".to_string(),
                    )
                })?)?;
            let transport_session_id =
                decode_publication_agent_string_field(parts.next().ok_or_else(|| {
                    LifecycleError::Protocol("publish_target is missing session field".to_string())
                })?)?;
            let source_session_name =
                decode_optional_agent_field(parts.next().ok_or_else(|| {
                    LifecycleError::Protocol(
                        "publish_target is missing source session field".to_string(),
                    )
                })?)?;
            let selector = decode_optional_agent_field(parts.next().ok_or_else(|| {
                LifecycleError::Protocol("publish_target is missing selector field".to_string())
            })?)?;
            let availability = parts.next().ok_or_else(|| {
                LifecycleError::Protocol("publish_target is missing availability field".to_string())
            })?;
            let session_role =
                decode_optional_static_agent_field(parts.next().ok_or_else(|| {
                    LifecycleError::Protocol(
                        "publish_target is missing session role field".to_string(),
                    )
                })?)?;
            let workspace_key = decode_optional_agent_field(parts.next().ok_or_else(|| {
                LifecycleError::Protocol(
                    "publish_target is missing workspace key field".to_string(),
                )
            })?)?;
            let command_name = decode_optional_agent_field(parts.next().ok_or_else(|| {
                LifecycleError::Protocol("publish_target is missing command name field".to_string())
            })?)?;
            let current_path = decode_optional_agent_field(parts.next().ok_or_else(|| {
                LifecycleError::Protocol("publish_target is missing current path field".to_string())
            })?)?;
            let attached_clients = parts
                .next()
                .ok_or_else(|| {
                    LifecycleError::Protocol(
                        "publish_target is missing attached clients field".to_string(),
                    )
                })?
                .parse::<usize>()
                .map_err(remote_target_publication_error)?;
            let window_count = parts
                .next()
                .ok_or_else(|| {
                    LifecycleError::Protocol(
                        "publish_target is missing window count field".to_string(),
                    )
                })?
                .parse::<usize>()
                .map_err(remote_target_publication_error)?;
            let task_state = parts.next().ok_or_else(|| {
                LifecycleError::Protocol("publish_target is missing task state field".to_string())
            })?;
            if parts.next().is_some() {
                return Err(LifecycleError::Protocol(
                    "publish_target contains unexpected extra fields".to_string(),
                ));
            }
            Ok(PublicationSenderCommand::PublishTarget {
                authority_id,
                transport_session_id,
                source_session_name,
                selector,
                availability: SessionAvailability::parse(availability)
                    .ok_or_else(|| {
                        LifecycleError::Protocol(format!(
                            "unsupported publication sender availability `{availability}`"
                        ))
                    })?
                    .as_str(),
                session_role,
                workspace_key,
                command_name,
                current_path,
                attached_clients,
                window_count,
                task_state: ManagedSessionTaskState::parse(task_state)
                    .ok_or_else(|| {
                        LifecycleError::Protocol(format!(
                            "unsupported publication sender task state `{task_state}`"
                        ))
                    })?
                    .as_str(),
            })
        }
        "exit_target" => {
            let authority_id =
                decode_publication_agent_string_field(parts.next().ok_or_else(|| {
                    LifecycleError::Protocol("exit_target is missing authority field".to_string())
                })?)?;
            let transport_session_id =
                decode_publication_agent_string_field(parts.next().ok_or_else(|| {
                    LifecycleError::Protocol("exit_target is missing session field".to_string())
                })?)?;
            let source_session_name =
                decode_optional_agent_field(parts.next().ok_or_else(|| {
                    LifecycleError::Protocol(
                        "exit_target is missing source session field".to_string(),
                    )
                })?)?;
            if parts.next().is_some() {
                return Err(LifecycleError::Protocol(
                    "exit_target contains unexpected extra fields".to_string(),
                ));
            }
            Ok(PublicationSenderCommand::ExitTarget {
                authority_id,
                transport_session_id,
                source_session_name,
            })
        }
        other => Err(LifecycleError::Protocol(format!(
            "unsupported remote publication sender command `{other}`"
        ))),
    }
}

fn decode_publication_agent_string_field(value: &str) -> Result<String, LifecycleError> {
    let bytes = decode_base64(value).map_err(remote_target_publication_error)?;
    String::from_utf8(bytes).map_err(remote_target_publication_error)
}

fn encode_optional_agent_field(value: Option<&str>) -> String {
    value
        .map(|value| encode_base64(value.as_bytes()))
        .unwrap_or_else(|| "~".to_string())
}

fn encode_optional_static_agent_field(value: Option<&'static str>) -> String {
    value
        .map(|value| encode_base64(value.as_bytes()))
        .unwrap_or_else(|| "~".to_string())
}

fn decode_optional_agent_field(value: &str) -> Result<Option<String>, LifecycleError> {
    if value == "~" {
        return Ok(None);
    }
    decode_publication_agent_string_field(value).map(Some)
}

fn decode_optional_static_agent_field(value: &str) -> Result<Option<&'static str>, LifecycleError> {
    decode_optional_agent_field(value)?
        .map(|value| {
            WorkspaceSessionRole::parse(&value)
                .map(|role| role.as_str())
                .ok_or_else(|| {
                    LifecycleError::Protocol(format!(
                        "unsupported publication sender session role `{value}`"
                    ))
                })
        })
        .transpose()
}

fn drain_pending_publication_agent_commands(
    listener: &UnixListener,
    commands: &mut Vec<PublicationAgentCommand>,
) -> Result<(), LifecycleError> {
    listener
        .set_nonblocking(true)
        .map_err(remote_target_publication_error)?;
    let result = drain_pending_publication_agent_commands_nonblocking(listener, commands);
    let reset = listener
        .set_nonblocking(false)
        .map_err(remote_target_publication_error);
    result?;
    reset
}

fn drain_pending_publication_agent_commands_nonblocking(
    listener: &UnixListener,
    commands: &mut Vec<PublicationAgentCommand>,
) -> Result<(), LifecycleError> {
    loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                if let Ok(command) = read_publication_agent_command(&mut stream) {
                    commands.push(command);
                }
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => return Ok(()),
            Err(error) => return Err(remote_target_publication_error(error)),
        }
    }
}

pub(crate) fn drain_pending_publication_sender_commands(
    listener: &UnixListener,
    commands: &mut Vec<PublicationSenderCommand>,
) -> Result<(), LifecycleError> {
    listener
        .set_nonblocking(true)
        .map_err(remote_target_publication_error)?;
    let result = drain_pending_publication_sender_commands_nonblocking(listener, commands);
    let reset = listener
        .set_nonblocking(false)
        .map_err(remote_target_publication_error);
    result?;
    reset
}

fn drain_pending_publication_sender_commands_nonblocking(
    listener: &UnixListener,
    commands: &mut Vec<PublicationSenderCommand>,
) -> Result<(), LifecycleError> {
    loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                if let Ok(command) = read_publication_sender_command(&mut stream) {
                    commands.push(command);
                }
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => return Ok(()),
            Err(error) => return Err(remote_target_publication_error(error)),
        }
    }
}

fn sanitize_path_component(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn publication_owner_snapshot(
    binding: &RemoteTargetPublicationBinding,
    local_target: &ManagedSessionRecord,
) -> PublicationOwnerSnapshot {
    PublicationOwnerSnapshot {
        authority_id: binding.authority_id.clone(),
        transport_session_id: binding.transport_session_id.clone(),
        selector: binding
            .selector
            .clone()
            .or_else(|| local_target.selector.clone()),
        availability: local_target.availability,
        workspace_key: local_target.workspace_key.clone(),
        session_role: local_target.session_role,
        attached_clients: local_target.attached_clients,
        window_count: local_target.window_count,
        command_name: local_target.command_name.clone(),
        current_path: local_target.current_path.clone(),
    }
}

fn publication_target_identity_changed(
    previous: &PublicationOwnerSnapshot,
    current: &PublicationOwnerSnapshot,
) -> bool {
    previous.authority_id != current.authority_id
        || previous.transport_session_id != current.transport_session_id
}

fn published_remote_target_from_local(
    binding: &RemoteTargetPublicationBinding,
    local_target: &ManagedSessionRecord,
) -> ManagedSessionRecord {
    ManagedSessionRecord {
        address: ManagedSessionAddress::remote_peer(
            binding.authority_id.clone(),
            binding.transport_session_id.clone(),
        ),
        selector: binding
            .selector
            .clone()
            .or_else(|| local_target.selector.clone()),
        availability: local_target.availability,
        workspace_dir: None,
        workspace_key: local_target.workspace_key.clone(),
        session_role: local_target.session_role,
        opened_by: Vec::new(),
        attached_clients: local_target.attached_clients,
        window_count: local_target.window_count,
        command_name: local_target.command_name.clone(),
        current_path: local_target.current_path.clone(),
        task_state: local_target.task_state,
    }
}

fn parse_publication_owner_command(
    line: &str,
) -> Result<Option<PublicationOwnerCommand>, LifecycleError> {
    match line.trim() {
        "" => Ok(None),
        "refresh" => Ok(Some(PublicationOwnerCommand::Refresh)),
        "stop" => Ok(Some(PublicationOwnerCommand::Stop)),
        other => Err(LifecycleError::Protocol(format!(
            "unsupported remote publication owner command `{other}`"
        ))),
    }
}

fn drain_publication_owner_commands(
    listener: &UnixListener,
) -> Result<PublicationOwnerDrain, LifecycleError> {
    let mut drain = PublicationOwnerDrain::default();
    loop {
        match listener.accept() {
            Ok((mut stream, _addr)) => {
                let mut buffer = String::new();
                stream
                    .read_to_string(&mut buffer)
                    .map_err(remote_target_publication_error)?;
                match parse_publication_owner_command(&buffer)? {
                    Some(PublicationOwnerCommand::Refresh) => drain.refresh_requested = true,
                    Some(PublicationOwnerCommand::Stop) => drain.stop_requested = true,
                    None => {}
                }
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => return Ok(drain),
            Err(error) => return Err(remote_target_publication_error(error)),
        }
    }
}

fn apply_publication_envelope(
    store: &PublishedTargetStore,
    source_socket_name: &str,
    envelope: &ProtocolEnvelope<ControlPlanePayload>,
) -> Result<bool, LifecycleError> {
    match &envelope.payload {
        ControlPlanePayload::TargetPublished(payload) => {
            let target = published_remote_target_record_from_payload(&envelope.sender_id, payload)?;
            store
                .upsert_target_from_source(
                    source_socket_name,
                    payload.source_session_name.as_deref(),
                    &target,
                )
                .map_err(remote_target_publication_error)
        }
        ControlPlanePayload::TargetExited(payload) => store
            .remove_target_from_source(
                source_socket_name,
                payload.source_session_name.as_deref(),
                &envelope.sender_id,
                &payload.transport_session_id,
            )
            .map_err(remote_target_publication_error),
        other => Err(LifecycleError::Protocol(format!(
            "unexpected remote target publication payload `{}`",
            other.message_type()
        ))),
    }
}

fn apply_discovered_remote_session_envelope(
    store: &DiscoveredRemoteSessionStore,
    node_id: &str,
    envelope: &ProtocolEnvelope<ControlPlanePayload>,
) -> Result<bool, LifecycleError> {
    match &envelope.payload {
        ControlPlanePayload::TargetPublished(payload) => {
            let session =
                published_remote_target_record_from_payload(&envelope.sender_id, payload)?;
            store
                .upsert_session_from_node(node_id, &session)
                .map_err(remote_target_publication_error)
        }
        ControlPlanePayload::TargetExited(payload) => store
            .remove_session_from_node(node_id, &envelope.sender_id, &payload.transport_session_id)
            .map_err(remote_target_publication_error),
        other => Err(LifecycleError::Protocol(format!(
            "unexpected discovered remote session payload `{}`",
            other.message_type()
        ))),
    }
}

struct DiscoveredRemoteSessionEnvelopeEffect {
    published_session: Option<ManagedSessionRecord>,
    exited_session: Option<(String, String)>,
}

fn discovered_remote_session_from_envelope(
    authority_id: &str,
    envelope: &ProtocolEnvelope<ControlPlanePayload>,
) -> Result<DiscoveredRemoteSessionEnvelopeEffect, LifecycleError> {
    match &envelope.payload {
        ControlPlanePayload::TargetPublished(payload) => {
            Ok(DiscoveredRemoteSessionEnvelopeEffect {
                published_session: Some(published_remote_target_record_from_payload(
                    &envelope.sender_id,
                    payload,
                )?),
                exited_session: None,
            })
        }
        ControlPlanePayload::TargetExited(payload) => Ok(DiscoveredRemoteSessionEnvelopeEffect {
            published_session: None,
            exited_session: Some((
                authority_id.to_string(),
                payload.transport_session_id.clone(),
            )),
        }),
        _ => Ok(DiscoveredRemoteSessionEnvelopeEffect {
            published_session: None,
            exited_session: None,
        }),
    }
}

fn mark_target_offline_in_store(
    store: &PublishedTargetStore,
    socket_name: &str,
    session_name: &str,
    target_id: &str,
) -> Result<bool, LifecycleError> {
    let records = store
        .list_records_for_source_binding(socket_name, session_name)
        .map_err(remote_target_publication_error)?;
    let mut changed = false;
    for record in records {
        if record.target.address.id().as_str() != target_id {
            continue;
        }
        let mut offline_target = record.target.clone();
        offline_target.availability = SessionAvailability::Offline;
        changed |= store
            .upsert_target_from_source(socket_name, Some(session_name), &offline_target)
            .map_err(remote_target_publication_error)?;
    }
    Ok(changed)
}

fn mark_discovered_remote_node_offline_in_store(
    store: &DiscoveredRemoteSessionStore,
    node_id: &str,
) -> Result<bool, LifecycleError> {
    store
        .mark_node_sessions_offline(node_id)
        .map_err(remote_target_publication_error)
}

fn spawn_socket_chrome_refresh(
    current_executable: &std::path::Path,
    socket_name: &str,
) -> Result<(), LifecycleError> {
    Command::new(current_executable)
        .args(chrome_refresh_socket_args(socket_name))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(remote_target_publication_error)
}

fn spawn_chrome_refresh_all(current_executable: &std::path::Path) -> Result<(), LifecycleError> {
    Command::new(current_executable)
        .arg("__chrome-refresh-all")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(remote_target_publication_error)
}

fn signal_remote_runtime_owner_upsert_all(
    owner: &RemoteRuntimeOwnerRuntime,
    node_id: &str,
    session: &ManagedSessionRecord,
) -> Result<bool, LifecycleError> {
    let socket_names = list_remote_runtime_owner_socket_names()?;
    for socket_name in &socket_names {
        owner.upsert_session(&socket_name, node_id, session)?;
    }
    Ok(!socket_names.is_empty())
}

fn signal_remote_runtime_owner_remove_all(
    owner: &RemoteRuntimeOwnerRuntime,
    node_id: &str,
    authority_id: &str,
    transport_session_id: &str,
) -> Result<bool, LifecycleError> {
    let socket_names = list_remote_runtime_owner_socket_names()?;
    for socket_name in &socket_names {
        owner.remove_session(&socket_name, node_id, authority_id, transport_session_id)?;
    }
    Ok(!socket_names.is_empty())
}

fn signal_remote_runtime_owner_mark_offline_all(
    owner: &RemoteRuntimeOwnerRuntime,
    node_id: &str,
) -> Result<bool, LifecycleError> {
    let socket_names = list_remote_runtime_owner_socket_names()?;
    for socket_name in &socket_names {
        owner.mark_node_offline(&socket_name, node_id)?;
    }
    Ok(!socket_names.is_empty())
}

fn list_remote_runtime_owner_socket_names() -> Result<Vec<String>, LifecycleError> {
    let mut socket_names = Vec::new();
    for entry in fs::read_dir(std::env::temp_dir()).map_err(remote_target_publication_error)? {
        let entry = entry.map_err(remote_target_publication_error)?;
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy();
        let Some(socket_name) = parse_remote_runtime_owner_socket_name(&name) else {
            continue;
        };
        socket_names.push(socket_name);
    }
    Ok(socket_names)
}

fn parse_remote_runtime_owner_socket_name(file_name: &str) -> Option<String> {
    file_name
        .strip_prefix("waitagent-remote-runtime-owner-")?
        .strip_suffix(".sock")
        .map(|value| value.to_string())
}

fn should_refresh_discovered_remote_catalog(
    discovered_store_changed: bool,
    owner_changed: bool,
) -> bool {
    discovered_store_changed || owner_changed
}

fn chrome_refresh_socket_args(socket_name: &str) -> Vec<String> {
    vec![
        "__chrome-refresh-socket".to_string(),
        "--socket-name".to_string(),
        socket_name.to_string(),
    ]
}

#[cfg(test)]
mod tests {
    use super::{
        apply_discovered_remote_session_envelope, chrome_refresh_socket_args,
        mark_discovered_remote_node_offline_in_store, mark_target_offline_in_store,
        parse_publication_agent_command, parse_publication_sender_command,
        publication_socket_hook_tmux_command, published_remote_target_from_local,
        published_remote_target_record_from_payload, remote_target_publication_agent_args,
        remote_target_publication_agent_socket_path, remote_target_publication_sender_args,
        remote_target_publication_sender_socket_path, remote_target_publication_server_args,
        render_publication_agent_command, render_publication_sender_command,
        should_refresh_discovered_remote_catalog, socket_lifecycle_publication_action,
        spawn_chrome_refresh_all, PublicationAgentCommand, PublicationSenderCommand,
        SocketLifecyclePublicationAction,
    };
    use crate::cli::RemoteNetworkConfig;
    use crate::domain::session_catalog::{
        ManagedSessionAddress, ManagedSessionRecord, ManagedSessionTaskState, SessionAvailability,
    };
    use crate::domain::workspace::WorkspaceSessionRole;
    use crate::infra::discovered_remote_session_store::DiscoveredRemoteSessionStore;
    use crate::infra::remote_protocol::{
        ControlPlanePayload, ProtocolEnvelope, TargetPublishedPayload,
    };
    use crate::infra::tmux::RemoteTargetPublicationBinding;
    use std::path::PathBuf;

    #[test]
    fn publication_record_uses_remote_identity_and_optional_selector() {
        let record = published_remote_target_record_from_payload(
            "peer-a",
            &TargetPublishedPayload {
                transport_session_id: "shell-1".to_string(),
                source_session_name: Some("target-host-1".to_string()),
                selector: Some("wa-local:shell-host".to_string()),
                availability: "online",
                session_role: Some("target-host"),
                workspace_key: Some("wk-1".to_string()),
                command_name: Some("codex".to_string()),
                current_path: Some("/tmp/demo".to_string()),
                attached_clients: 2,
                window_count: 3,
                task_state: "confirm",
            },
        )
        .expect("publication payload should build a published record");

        assert_eq!(record.address.authority_id(), "peer-a");
        assert_eq!(record.address.session_id(), "shell-1");
        assert_eq!(record.selector.as_deref(), Some("wa-local:shell-host"));
        assert_eq!(record.availability, SessionAvailability::Online);
        assert_eq!(record.session_role, Some(WorkspaceSessionRole::TargetHost));
        assert_eq!(record.current_path, Some(PathBuf::from("/tmp/demo")));
        assert_eq!(record.task_state, ManagedSessionTaskState::Confirm);
    }

    #[test]
    fn publication_record_rejects_unknown_availability() {
        let error = published_remote_target_record_from_payload(
            "peer-a",
            &TargetPublishedPayload {
                transport_session_id: "shell-1".to_string(),
                source_session_name: None,
                selector: None,
                availability: "weird",
                session_role: None,
                workspace_key: None,
                command_name: None,
                current_path: None,
                attached_clients: 0,
                window_count: 1,
                task_state: "unknown",
            },
        )
        .expect_err("unknown availability should fail");

        assert!(error
            .to_string()
            .contains("unsupported remote target availability"));
    }

    #[test]
    fn publication_server_args_target_hidden_listener_command() {
        assert_eq!(
            remote_target_publication_server_args("wa-local", &RemoteNetworkConfig::default()),
            vec![
                "--port",
                "7474",
                "__remote-target-publication-server",
                "--socket-name",
                "wa-local",
            ]
        );
    }

    #[test]
    fn publication_agent_args_target_hidden_listener_command() {
        assert_eq!(
            remote_target_publication_agent_args("wa-local", &RemoteNetworkConfig::default()),
            vec![
                "--port",
                "7474",
                "__remote-target-publication-agent",
                "--socket-name",
                "wa-local",
            ]
        );
    }

    #[test]
    fn publication_agent_socket_path_is_scoped_to_socket_name() {
        let path = remote_target_publication_agent_socket_path("wa/local");

        assert!(path
            .to_string_lossy()
            .contains("waitagent-remote-publication-agent-wa_local.sock"));
    }

    #[test]
    fn publication_sender_args_target_hidden_listener_command() {
        assert_eq!(
            remote_target_publication_sender_args("wa-local", &RemoteNetworkConfig::default()),
            vec![
                "--port",
                "7474",
                "__remote-target-publication-sender",
                "--socket-name",
                "wa-local",
            ]
        );
    }

    #[test]
    fn publication_sender_socket_path_is_scoped_to_socket_name() {
        let path = remote_target_publication_sender_socket_path("wa/local");

        assert!(path
            .to_string_lossy()
            .contains("waitagent-remote-publication-sender-wa_local.sock"));
    }

    #[test]
    fn publication_agent_command_round_trips_publish_session() {
        let rendered = render_publication_agent_command(&PublicationAgentCommand::PublishSession {
            session_name: "waitagent-target-1".to_string(),
        });

        let parsed =
            parse_publication_agent_command(rendered.trim()).expect("command should decode");

        assert_eq!(
            parsed,
            PublicationAgentCommand::PublishSession {
                session_name: "waitagent-target-1".to_string(),
            }
        );
    }

    #[test]
    fn publication_agent_command_round_trips_exit_target() {
        let rendered = render_publication_agent_command(&PublicationAgentCommand::ExitTarget {
            authority_id: "peer-a".to_string(),
            transport_session_id: "shell-1".to_string(),
            source_session_name: Some("target-host-1".to_string()),
        });

        let parsed =
            parse_publication_agent_command(rendered.trim()).expect("command should decode");

        assert_eq!(
            parsed,
            PublicationAgentCommand::ExitTarget {
                authority_id: "peer-a".to_string(),
                transport_session_id: "shell-1".to_string(),
                source_session_name: Some("target-host-1".to_string()),
            }
        );
    }

    #[test]
    fn publication_sender_command_round_trips_publish_target() {
        let rendered =
            render_publication_sender_command(&PublicationSenderCommand::PublishTarget {
                authority_id: "peer-a".to_string(),
                transport_session_id: "shell-1".to_string(),
                source_session_name: Some("target-host-1".to_string()),
                selector: Some("wa-local:target-host-1".to_string()),
                availability: "online",
                session_role: Some("target-host"),
                workspace_key: Some("wk-1".to_string()),
                command_name: Some("codex".to_string()),
                current_path: Some("/tmp/demo".to_string()),
                attached_clients: 2,
                window_count: 3,
                task_state: "running",
            });

        let parsed =
            parse_publication_sender_command(rendered.trim()).expect("command should decode");

        assert_eq!(
            parsed,
            PublicationSenderCommand::PublishTarget {
                authority_id: "peer-a".to_string(),
                transport_session_id: "shell-1".to_string(),
                source_session_name: Some("target-host-1".to_string()),
                selector: Some("wa-local:target-host-1".to_string()),
                availability: "online",
                session_role: Some("target-host"),
                workspace_key: Some("wk-1".to_string()),
                command_name: Some("codex".to_string()),
                current_path: Some("/tmp/demo".to_string()),
                attached_clients: 2,
                window_count: 3,
                task_state: "running",
            }
        );
    }

    #[test]
    fn publication_sender_command_round_trips_register_live_session() {
        let rendered =
            render_publication_sender_command(&PublicationSenderCommand::RegisterLiveSession {
                target_session_name: "target-host-1".to_string(),
                authority_id: "peer-a".to_string(),
                target_id: "remote-peer:peer-a:target-host-1".to_string(),
                transport_socket_path: "/tmp/waitagent-remote.sock".to_string(),
            });

        let parsed =
            parse_publication_sender_command(rendered.trim()).expect("command should decode");

        assert_eq!(
            parsed,
            PublicationSenderCommand::RegisterLiveSession {
                target_session_name: "target-host-1".to_string(),
                authority_id: "peer-a".to_string(),
                target_id: "remote-peer:peer-a:target-host-1".to_string(),
                transport_socket_path: "/tmp/waitagent-remote.sock".to_string(),
            }
        );
    }

    #[test]
    fn mark_target_offline_in_store_keeps_record_and_updates_availability() {
        let store_path = std::env::temp_dir().join(format!(
            "waitagent-publication-offline-test-{}-{}.txt",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock should be after unix epoch")
                .as_nanos()
        ));
        let store = crate::infra::published_target_store::PublishedTargetStore::new(&store_path);
        let target = ManagedSessionRecord {
            address: ManagedSessionAddress::remote_peer("peer-a", "shell-1"),
            selector: Some("wa-local:target-host-1".to_string()),
            availability: SessionAvailability::Online,
            workspace_dir: None,
            workspace_key: Some("wk-1".to_string()),
            session_role: Some(WorkspaceSessionRole::TargetHost),
            opened_by: Vec::new(),
            attached_clients: 2,
            window_count: 1,
            command_name: Some("codex".to_string()),
            current_path: Some(PathBuf::from("/tmp/demo")),
            task_state: ManagedSessionTaskState::Unknown,
        };
        store
            .upsert_target_from_source("wa-local", Some("target-host-1"), &target)
            .expect("target should store");

        let changed = mark_target_offline_in_store(
            &store,
            "wa-local",
            "target-host-1",
            "remote-peer:peer-a:shell-1",
        )
        .expect("offline mark should succeed");

        assert!(changed);
        let record = store
            .list_records_for_source_binding("wa-local", "target-host-1")
            .expect("stored record should load")
            .into_iter()
            .next()
            .expect("record should remain present");
        assert_eq!(record.target.availability, SessionAvailability::Offline);

        let _ = std::fs::remove_file(store_path);
    }

    #[test]
    fn discovered_publication_envelope_upserts_remote_session_for_node() {
        let store_path = std::env::temp_dir().join(format!(
            "waitagent-discovered-publication-test-{}-{}.txt",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock should be after unix epoch")
                .as_nanos()
        ));
        let store = DiscoveredRemoteSessionStore::new(&store_path);

        let changed = apply_discovered_remote_session_envelope(
            &store,
            "peer-a",
            &ProtocolEnvelope {
                protocol_version: "1".to_string(),
                message_id: "msg-1".to_string(),
                message_type: "target_published",
                timestamp: "0Z".to_string(),
                sender_id: "peer-a".to_string(),
                correlation_id: None,
                session_id: Some("shell-1".to_string()),
                target_id: Some("remote-peer:peer-a:shell-1".to_string()),
                attachment_id: None,
                console_id: None,
                payload: ControlPlanePayload::TargetPublished(TargetPublishedPayload {
                    transport_session_id: "shell-1".to_string(),
                    source_session_name: None,
                    selector: Some("wa-peer-a:shell-1".to_string()),
                    availability: "online",
                    session_role: Some("target-host"),
                    workspace_key: Some("wk-1".to_string()),
                    command_name: Some("codex".to_string()),
                    current_path: Some("/tmp/demo".to_string()),
                    attached_clients: 2,
                    window_count: 1,
                    task_state: "input",
                }),
            },
        )
        .expect("discovered publication should apply");

        assert!(changed);
        let record = store
            .list_records_for_node("peer-a")
            .expect("records should load")
            .into_iter()
            .next()
            .expect("record should exist");
        assert_eq!(record.session.address.qualified_target(), "peer-a:shell-1");
        assert_eq!(record.session.command_name.as_deref(), Some("codex"));

        let _ = std::fs::remove_file(store_path);
    }

    #[test]
    fn discovered_node_offline_keeps_record_and_updates_availability() {
        let store_path = std::env::temp_dir().join(format!(
            "waitagent-discovered-offline-test-{}-{}.txt",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock should be after unix epoch")
                .as_nanos()
        ));
        let store = DiscoveredRemoteSessionStore::new(&store_path);
        let session = ManagedSessionRecord {
            address: ManagedSessionAddress::remote_peer("peer-a", "shell-1"),
            selector: Some("wa-peer-a:shell-1".to_string()),
            availability: SessionAvailability::Online,
            workspace_dir: None,
            workspace_key: Some("wk-1".to_string()),
            session_role: Some(WorkspaceSessionRole::TargetHost),
            opened_by: Vec::new(),
            attached_clients: 1,
            window_count: 1,
            command_name: Some("bash".to_string()),
            current_path: Some(PathBuf::from("/tmp/demo")),
            task_state: ManagedSessionTaskState::Unknown,
        };
        store
            .upsert_session_from_node("peer-a", &session)
            .expect("session should store");

        let changed = mark_discovered_remote_node_offline_in_store(&store, "peer-a")
            .expect("offline should apply");

        assert!(changed);
        let record = store
            .list_records_for_node("peer-a")
            .expect("records should load")
            .into_iter()
            .next()
            .expect("record should remain");
        assert_eq!(record.session.availability, SessionAvailability::Offline);

        let _ = std::fs::remove_file(store_path);
    }

    #[test]
    fn discovered_remote_catalog_refreshes_when_owner_state_changes() {
        assert!(should_refresh_discovered_remote_catalog(false, true));
        assert!(should_refresh_discovered_remote_catalog(true, false));
        assert!(!should_refresh_discovered_remote_catalog(false, false));
    }

    #[test]
    fn publication_agent_command_rejects_unknown_opcode() {
        let error = parse_publication_agent_command("weird")
            .expect_err("unknown publication agent command should fail");

        assert!(error
            .to_string()
            .contains("unsupported remote publication agent command"));
    }

    #[test]
    fn chrome_refresh_socket_args_target_hidden_socket_refresh_command() {
        assert_eq!(
            chrome_refresh_socket_args("wa-local"),
            vec!["__chrome-refresh-socket", "--socket-name", "wa-local"]
        );
    }

    #[test]
    fn chrome_refresh_all_spawn_returns_hidden_all_refresh_command() {
        let temp = std::env::temp_dir().join("waitagent-nonexistent-refresh-all-bin");
        let error =
            spawn_chrome_refresh_all(&temp).expect_err("missing executable should return an error");

        assert!(error
            .to_string()
            .contains("failed to update published remote target catalog"));
    }

    #[test]
    fn publication_socket_hook_tmux_command_targets_reconcile_and_socket_refresh() {
        let command = publication_socket_hook_tmux_command("/tmp/wait agent", "wa-local");

        assert_eq!(
            command,
            "run-shell -b \"'/tmp/wait agent' '__socket-lifecycle-hook' '--socket-name' 'wa-local' '--hook-name' '#{hook}' '--session-name' '#{hook_session_name}' >/dev/null 2>&1\""
        );
    }

    #[test]
    fn client_lifecycle_hooks_prefer_targeted_publish() {
        assert_eq!(
            socket_lifecycle_publication_action(Some("client-attached")),
            SocketLifecyclePublicationAction::TargetedPublish
        );
        assert_eq!(
            socket_lifecycle_publication_action(Some("client-detached")),
            SocketLifecyclePublicationAction::TargetedPublish
        );
        assert_eq!(
            socket_lifecycle_publication_action(Some("session-created")),
            SocketLifecyclePublicationAction::TargetedPublish
        );
    }

    #[test]
    fn session_closed_hook_prefers_targeted_exit() {
        assert_eq!(
            socket_lifecycle_publication_action(Some("session-closed")),
            SocketLifecyclePublicationAction::TargetedExit
        );
    }

    #[test]
    fn unknown_hook_falls_back_to_full_reconcile() {
        assert_eq!(
            socket_lifecycle_publication_action(Some("weird-hook")),
            SocketLifecyclePublicationAction::FullReconcile
        );
    }

    #[test]
    fn reconcile_projects_local_target_host_as_remote_peer() {
        let published = published_remote_target_from_local(
            &RemoteTargetPublicationBinding {
                socket_name: "wa-local".to_string(),
                target_session_name: "shell-host".to_string(),
                authority_id: "peer-a".to_string(),
                transport_session_id: "shell-1".to_string(),
                selector: Some("wa-local:shell-host".to_string()),
            },
            &ManagedSessionRecord {
                address: ManagedSessionAddress::local_tmux("wa-local", "shell-host"),
                selector: Some("wa-local:shell-host".to_string()),
                availability: SessionAvailability::Online,
                workspace_dir: Some(PathBuf::from("/tmp/demo")),
                workspace_key: Some("wk-1".to_string()),
                session_role: Some(WorkspaceSessionRole::TargetHost),
                opened_by: Vec::new(),
                attached_clients: 1,
                window_count: 2,
                command_name: Some("codex".to_string()),
                current_path: Some(PathBuf::from("/tmp/demo")),
                task_state: ManagedSessionTaskState::Running,
            },
        );

        assert_eq!(published.address.authority_id(), "peer-a");
        assert_eq!(published.address.session_id(), "shell-1");
        assert_eq!(published.selector.as_deref(), Some("wa-local:shell-host"));
        assert_eq!(published.command_name.as_deref(), Some("codex"));
        assert_eq!(published.current_path, Some(PathBuf::from("/tmp/demo")));
        assert_eq!(published.workspace_dir, None);
    }
}
