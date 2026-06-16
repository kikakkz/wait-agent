use crate::cli::{
    prepend_global_network_args, RemoteAuthorityTargetHostCommand, RemoteNetworkConfig,
    RemoteSessionSyncOwnerCommand,
};
use crate::domain::session_catalog::ManagedSessionRecord;
use crate::infra::error_log::ERROR_LOG;
use crate::infra::published_target_store::PublishedTargetStore;
use crate::infra::remote_grpc_transport::{
    GrpcRemoteNodeTransport, GrpcRemoteNodeTransportGuard, OutboundNodeSessionRequest,
    RemoteNodeSessionHandle, RemoteNodeTransport, RemoteNodeTransportEvent,
};
use crate::infra::tmux::{EmbeddedTmuxBackend, TmuxChromeGateway, TmuxSocketName};
use crate::lifecycle::LifecycleError;
use crate::runtime::current_executable::current_waitagent_executable;
use crate::runtime::remote_authority_target_host_runtime::RemoteAuthorityPublicationGateway;
use crate::runtime::remote_authority_transport_runtime::RemoteAuthorityCommand;
use crate::runtime::remote_node_session_owner_runtime::live_authority_session_socket_path;
use crate::runtime::remote_node_session_runtime::GrpcAuthorityEvent;
use crate::runtime::sidecar_process_runtime::spawn_waitagent_sidecar;
use std::collections::HashMap;
use std::fs;
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

mod sync_helpers;
pub(crate) use sync_helpers::*;

const SESSION_SYNC_POLL_INTERVAL: Duration = Duration::from_millis(500);
const SESSION_SYNC_RECONNECT_DELAY: Duration = Duration::from_millis(500);

const REMOTE_SESSION_SYNC_OWNER_READY_RETRIES: usize = 20;
const REMOTE_SESSION_SYNC_OWNER_READY_SLEEP: Duration = Duration::from_millis(25);
pub(super) const SESSION_SYNC_AUTHORITY_ID: &str = "waitagent-session-sync-authority";
pub(super) const LIVE_AUTHORITY_SERVER_ID: &str = "waitagent-live-authority-owner";
pub(super) const WAITAGENT_ACTIVE_TARGET_OPTION: &str = "@waitagent_active_target";

pub trait LocalSessionCatalog: Send + 'static {
    type Error: ToString;

    fn list_local_sessions(&self) -> Result<Vec<ManagedSessionRecord>, Self::Error>;
}

#[derive(Clone)]
pub struct SocketScopedLocalSessionCatalog<G> {
    gateway: G,
    socket_name: TmuxSocketName,
    published_target_store: PublishedTargetStore,
}

impl<G> SocketScopedLocalSessionCatalog<G> {
    pub fn new(
        gateway: G,
        socket_name: TmuxSocketName,
        published_target_store: PublishedTargetStore,
    ) -> Self {
        Self {
            gateway,
            socket_name,
            published_target_store,
        }
    }
}

impl<G> LocalSessionCatalog for SocketScopedLocalSessionCatalog<G>
where
    G: TmuxChromeGateway + Send + 'static,
    G::Error: ToString,
{
    type Error = G::Error;

    fn list_local_sessions(&self) -> Result<Vec<ManagedSessionRecord>, Self::Error> {
        let sessions = self.gateway.list_sessions_on_socket(&self.socket_name)?;
        let active_targets =
            active_workspace_targets_on_socket(&self.gateway, &self.socket_name, &sessions)?;
        Ok(exportable_local_sessions_for_socket(
            overlay_workspace_runtime_onto_active_local_target_hosts(
                sessions,
                self.socket_name.as_str(),
                &active_targets,
            ),
            self.socket_name.as_str(),
            &self.published_target_store,
        ))
    }
}

pub trait OutboundRemoteNodeTransport: Clone + Send + 'static {
    type Guard: Send + 'static;
    type Error: ToString;

    fn connect_outbound(
        &self,
        request: OutboundNodeSessionRequest,
        event_tx: mpsc::Sender<RemoteNodeTransportEvent>,
    ) -> Result<Self::Guard, Self::Error>;
}

impl OutboundRemoteNodeTransport for GrpcRemoteNodeTransport {
    type Guard = GrpcRemoteNodeTransportGuard;
    type Error = crate::infra::remote_grpc_transport::RemoteNodeTransportError;

    fn connect_outbound(
        &self,
        request: OutboundNodeSessionRequest,
        event_tx: mpsc::Sender<RemoteNodeTransportEvent>,
    ) -> Result<Self::Guard, Self::Error> {
        RemoteNodeTransport::connect_outbound(self, request, event_tx)
    }
}

pub trait LocalTargetExitObserver: Clone + Send + 'static {
    fn observe_local_target_exit(
        &self,
        socket_name: &str,
        target_session_name: &str,
    ) -> Result<(), LifecycleError>;
}

#[derive(Clone)]
pub struct SidecarLocalTargetExitObserver {
    network: RemoteNetworkConfig,
    current_executable: PathBuf,
}

impl SidecarLocalTargetExitObserver {
    pub fn from_build_env(network: RemoteNetworkConfig) -> Result<Self, LifecycleError> {
        Ok(Self {
            network,
            current_executable: current_waitagent_executable()?,
        })
    }
}

impl LocalTargetExitObserver for SidecarLocalTargetExitObserver {
    fn observe_local_target_exit(
        &self,
        socket_name: &str,
        target_session_name: &str,
    ) -> Result<(), LifecycleError> {
        let args = prepend_global_network_args(
            vec![
                "__local-target-exited".to_string(),
                "--socket-name".to_string(),
                socket_name.to_string(),
                "--target-session-name".to_string(),
                target_session_name.to_string(),
                "--pane-id".to_string(),
                String::new(),
            ],
            &self.network,
        );
        spawn_waitagent_sidecar(&self.current_executable, args).map_err(remote_session_sync_error)
    }
}

pub struct RemoteNodeSessionSyncRuntime<
    G,
    T = GrpcRemoteNodeTransport,
    O = SidecarLocalTargetExitObserver,
> {
    gateway: G,
    transport: T,
    local_target_exit_observer: O,
    network: RemoteNetworkConfig,
    poll_interval: Duration,
    reconnect_delay: Duration,
}

pub struct RemoteNodeSessionSyncGuard {
    stop_tx: Option<mpsc::Sender<()>>,
    worker: Option<thread::JoinHandle<()>>,
}

pub(super) struct SessionSyncAuthorityManager {
    pub(super) running_hosts: HashMap<String, SessionSyncAuthorityHost>,
}

pub(super) struct SessionSyncAuthorityHost {
    pub(super) writer: Arc<Mutex<Option<UnixStream>>>,
    pub(super) running: Arc<AtomicBool>,
    pub(super) writer_ready: Arc<Condvar>,
}

#[derive(Clone, Default)]
pub(super) struct NoopAuthorityPublicationGateway;

impl RemoteNodeSessionSyncRuntime<SocketScopedLocalSessionCatalog<EmbeddedTmuxBackend>> {
    pub fn from_build_env_with_network_and_socket(
        network: RemoteNetworkConfig,
        socket_name: &str,
    ) -> Result<Self, LifecycleError> {
        Ok(Self::new_with_local_target_exit_observer(
            SocketScopedLocalSessionCatalog::new(
                EmbeddedTmuxBackend::from_build_env().map_err(remote_session_sync_error)?,
                TmuxSocketName::new(socket_name),
                PublishedTargetStore::new(std::path::PathBuf::from("/dev/null")),
            ),
            GrpcRemoteNodeTransport::new(),
            SidecarLocalTargetExitObserver::from_build_env(network.clone())?,
            network,
        ))
    }

    pub fn run_owner(
        command: RemoteSessionSyncOwnerCommand,
        network: RemoteNetworkConfig,
    ) -> Result<(), LifecycleError> {
        // Install a file-based panic hook so that panic messages are captured
        // even when stderr goes to a deleted pts (the sidecar's original stderr
        // fd may reference a dead pty). The hook chains to the original hook so
        // stderr output is preserved when it is available.
        let socket_name_for_hook = command.socket_name.clone();
        let original_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let diag_path = std::env::temp_dir().join(format!(
                "waitagent-sync-owner-panic-{}.log",
                socket_name_for_hook
                    .chars()
                    .map(|ch| match ch {
                        'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' => ch,
                        _ => '_',
                    })
                    .collect::<String>()
            ));
            let _ = std::fs::write(
                &diag_path,
                format!("remote session sync owner panicked: {info}\n"),
            );
            original_hook(info);
        }));

        let socket_path = remote_session_sync_owner_socket_path(&command.socket_name);
        if socket_path.exists() {
            let _ = fs::remove_file(&socket_path);
        }
        let listener = UnixListener::bind(&socket_path).map_err(remote_session_sync_error)?;
        listener
            .set_nonblocking(true)
            .map_err(remote_session_sync_error)?;
        let _guard =
            Self::from_build_env_with_network_and_socket(network, &command.socket_name)?.start()?;
        while backend_socket_still_exists(&command.socket_name) {
            let _ = drain_owner_ping(&listener);
            thread::sleep(SESSION_SYNC_POLL_INTERVAL);
        }
        let _ = fs::remove_file(&socket_path);
        Ok(())
    }

    pub fn ensure_owner_running(
        socket_name: &str,
        network: &RemoteNetworkConfig,
    ) -> Result<(), LifecycleError> {
        let socket_path = remote_session_sync_owner_socket_path(socket_name);
        if remote_session_sync_owner_available(&socket_path) {
            return Ok(());
        }
        if socket_path.exists() {
            let _ = fs::remove_file(&socket_path);
        }
        let current_executable = current_waitagent_executable()?;
        spawn_waitagent_sidecar(
            &current_executable,
            remote_session_sync_owner_args(socket_name, network),
        )
        .map_err(remote_session_sync_error)?;
        for _ in 0..REMOTE_SESSION_SYNC_OWNER_READY_RETRIES {
            if remote_session_sync_owner_available(&socket_path) {
                return Ok(());
            }
            thread::sleep(REMOTE_SESSION_SYNC_OWNER_READY_SLEEP);
        }
        Err(LifecycleError::Protocol(format!(
            "remote session sync owner for socket `{socket_name}` did not become ready"
        )))
    }
}

impl<G, T, O> RemoteNodeSessionSyncRuntime<G, T, O>
where
    G: LocalSessionCatalog,
    T: OutboundRemoteNodeTransport,
    O: LocalTargetExitObserver,
{
    pub fn new_with_local_target_exit_observer(
        gateway: G,
        transport: T,
        local_target_exit_observer: O,
        network: RemoteNetworkConfig,
    ) -> Self {
        Self {
            gateway,
            transport,
            local_target_exit_observer,
            network,
            poll_interval: SESSION_SYNC_POLL_INTERVAL,
            reconnect_delay: SESSION_SYNC_RECONNECT_DELAY,
        }
    }

    pub fn start(self) -> Result<RemoteNodeSessionSyncGuard, LifecycleError> {
        let endpoint_uri = self.network.connect_endpoint_uri().ok_or_else(|| {
            LifecycleError::Protocol("remote session sync requires `--connect`".to_string())
        })?;
        let node_id = self.network.advertised_node_id();
        let (stop_tx, stop_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            run_remote_session_sync_loop(
                self.gateway,
                self.transport,
                self.network,
                self.local_target_exit_observer,
                node_id,
                endpoint_uri,
                self.poll_interval,
                self.reconnect_delay,
                stop_rx,
            );
        });
        Ok(RemoteNodeSessionSyncGuard {
            stop_tx: Some(stop_tx),
            worker: Some(worker),
        })
    }
}

impl SessionSyncAuthorityManager {
    pub(super) fn new() -> Self {
        Self {
            running_hosts: HashMap::new(),
        }
    }

    pub(super) fn shutdown(&mut self) {
        for (_, host) in self.running_hosts.drain() {
            host.running.store(false, Ordering::Relaxed);
            let writer = match host.writer.lock() {
                Ok(mut guard) => guard.take(),
                Err(poisoned) => {
                    ERROR_LOG.log(
                        "[session-sync] authority writer mutex poisoned, recovering".to_string(),
                    );
                    poisoned.into_inner().take()
                }
            };
            if let Some(writer) = writer {
                let _ = writer.shutdown(Shutdown::Both);
            }
        }
    }

    pub(super) fn handle_event(
        &mut self,
        session_handle: &RemoteNodeSessionHandle,
        event: GrpcAuthorityEvent,
    ) {
        match event {
            GrpcAuthorityEvent::Command(command) => {
                if let Err(error) = self.ensure_and_send_command(session_handle, command) {
                    ERROR_LOG.log(format!(
                        "[session-sync] failed to handle authority command: {error}"
                    ));
                }
            }
            GrpcAuthorityEvent::CreateSessionRequest { .. }
            | GrpcAuthorityEvent::CreateSessionAccepted(_)
            | GrpcAuthorityEvent::CreateSessionRejected(_)
            | GrpcAuthorityEvent::MirrorAccepted
            | GrpcAuthorityEvent::MirrorRejected(_)
            | GrpcAuthorityEvent::Failed(_)
            | GrpcAuthorityEvent::Closed => {}
        }
    }

    fn ensure_authority_host(
        &mut self,
        session_handle: &RemoteNodeSessionHandle,
        target_id: &str,
    ) -> Result<(), LifecycleError> {
        let session_name = target_session_name_from_target_id(target_id).ok_or_else(|| {
            ERROR_LOG.log(format!(
                "[session-sync] failed to extract session from target id `{target_id}`"
            ));
            LifecycleError::Protocol(format!(
                "failed to derive local session from target id `{target_id}`"
            ))
        })?;
        let socket_name = find_socket_for_session(&session_name).ok_or_else(|| {
            ERROR_LOG.log(format!(
                "[session-sync] no local socket owns session `{session_name}` for `{target_id}`"
            ));
            LifecycleError::Protocol(format!(
                "no local workspace socket owns session `{session_name}` for `{target_id}`"
            ))
        })?;
        let authority_socket_path = live_authority_session_socket_path(&socket_name, &session_name);
        let transport_socket_path = remote_session_sync_owner_socket_path(&socket_name);
        let running = Arc::new(AtomicBool::new(true));
        let writer = Arc::new(Mutex::new(None));
        let writer_ready = Arc::new(Condvar::new());
        spawn_live_authority_listener(
            authority_socket_path.clone(),
            session_handle.clone(),
            running.clone(),
            writer.clone(),
            writer_ready.clone(),
        );
        spawn_in_process_authority_target_host(
            running.clone(),
            writer.clone(),
            writer_ready.clone(),
            RemoteAuthorityTargetHostCommand {
                socket_name: socket_name.clone(),
                target_session_name: session_name.clone(),
                transport_session_id: target_session_name_from_target_id(target_id)
                    .unwrap_or_else(|| target_id.to_string()),
                authority_id: session_handle.node_id().to_string(),
                target_id: target_id.to_string(),
                transport_socket_path: transport_socket_path.to_string_lossy().into_owned(),
            },
        )?;
        self.running_hosts.insert(
            target_id.to_string(),
            SessionSyncAuthorityHost {
                writer,
                running,
                writer_ready,
            },
        );
        Ok(())
    }

    fn ensure_and_send_command(
        &mut self,
        session_handle: &RemoteNodeSessionHandle,
        command: RemoteAuthorityCommand,
    ) -> Result<(), LifecycleError> {
        let target_id = authority_command_target_id(&command).to_string();
        if !self.running_hosts.contains_key(&target_id) {
            self.ensure_authority_host(session_handle, &target_id)?;
        }
        self.deliver_with_host_rebuild(session_handle, &target_id, command, false)
    }

    fn deliver_with_host_rebuild(
        &mut self,
        session_handle: &RemoteNodeSessionHandle,
        target_id: &str,
        command: RemoteAuthorityCommand,
        rebuilt: bool,
    ) -> Result<(), LifecycleError> {
        let signal = self
            .running_hosts
            .get(target_id)
            .map(authority_host_signal)
            .unwrap_or(AuthorityHostSignal::Closed);

        if matches!(signal, AuthorityHostSignal::Closed) {
            if rebuilt {
                return Err(LifecycleError::Protocol(format!(
                    "authority host for `{target_id}` closed before accepting command"
                )));
            }
            self.running_hosts.remove(target_id);
            self.ensure_authority_host(session_handle, target_id)?;
            return self.deliver_with_host_rebuild(session_handle, target_id, command, true);
        }

        let host = self.running_hosts.get(target_id).ok_or_else(|| {
            LifecycleError::Protocol("authority host cache lost entry".to_string())
        })?;
        match deliver_command_to_ready_host(host, command.clone())? {
            AuthorityHostSignal::Ready => Ok(()),
            AuthorityHostSignal::Starting => Err(LifecycleError::Protocol(format!(
                "authority host for `{target_id}` did not become ready"
            ))),
            AuthorityHostSignal::Closed => {
                if rebuilt {
                    self.running_hosts.remove(target_id);
                    return Err(LifecycleError::Protocol(format!(
                        "authority host for `{target_id}` closed before accepting command"
                    )));
                }
                self.running_hosts.remove(target_id);
                self.ensure_authority_host(session_handle, target_id)?;
                self.deliver_with_host_rebuild(session_handle, target_id, command, true)
            }
        }
    }
}

impl RemoteAuthorityPublicationGateway for NoopAuthorityPublicationGateway {
    fn ensure_live_session_registered(
        &self,
        socket_name: &str,
        target_session_name: &str,
        _authority_id: &str,
        _target_id: &str,
        _transport_socket_path: &str,
    ) -> Result<PathBuf, LifecycleError> {
        let authority_socket_path =
            live_authority_session_socket_path(socket_name, target_session_name);
        wait_for_live_authority_socket(&authority_socket_path)?;
        Ok(authority_socket_path)
    }

    fn ensure_live_session_unregistered(
        &self,
        _socket_name: &str,
        _target_session_name: &str,
    ) -> Result<(), LifecycleError> {
        Ok(())
    }

    fn signal_source_session_closed(
        &self,
        _socket_name: &str,
        _target_session_name: &str,
    ) -> Result<(), LifecycleError> {
        Ok(())
    }
}

impl Drop for RemoteNodeSessionSyncGuard {
    fn drop(&mut self) {
        if let Some(stop_tx) = self.stop_tx.take() {
            let _ = stop_tx.send(());
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod remote_node_session_sync_runtime_test;
