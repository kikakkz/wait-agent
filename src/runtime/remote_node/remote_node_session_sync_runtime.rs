use crate::cli::{
    RemoteAuthorityTargetHostCommand, RemoteNetworkConfig, RemoteSessionSyncOwnerCommand,
};
use crate::domain::session_catalog::ManagedSessionRecord;
use crate::infra::published_target_store::PublishedTargetStore;
use crate::infra::remote_grpc_transport::{
    GrpcRemoteNodeTransport, GrpcRemoteNodeTransportGuard, OutboundNodeSessionRequest,
    RemoteNodeSessionHandle, RemoteNodeTransport, RemoteNodeTransportEvent,
};
use crate::infra::tmux::{EmbeddedTmuxBackend, TmuxChromeGateway, TmuxSocketName};
use crate::lifecycle::LifecycleError;
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
const SESSION_SYNC_RAW_INPUT_QUIET_WINDOW: Duration = Duration::from_millis(750);
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
    pub fn new(gateway: G, socket_name: TmuxSocketName) -> Self {
        Self {
            gateway,
            socket_name,
            published_target_store: PublishedTargetStore::default(),
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

pub struct RemoteNodeSessionSyncRuntime<G, T = GrpcRemoteNodeTransport> {
    gateway: G,
    transport: T,
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
        Ok(Self::new(
            SocketScopedLocalSessionCatalog::new(
                EmbeddedTmuxBackend::from_build_env().map_err(remote_session_sync_error)?,
                TmuxSocketName::new(socket_name),
            ),
            GrpcRemoteNodeTransport::new(),
            network,
        ))
    }

    pub fn run_owner(
        command: RemoteSessionSyncOwnerCommand,
        network: RemoteNetworkConfig,
    ) -> Result<(), LifecycleError> {
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
        let current_executable = std::env::current_exe().map_err(|error| {
            LifecycleError::Io(
                "failed to locate current waitagent executable".to_string(),
                error,
            )
        })?;
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

impl<G, T> RemoteNodeSessionSyncRuntime<G, T>
where
    G: LocalSessionCatalog,
    T: OutboundRemoteNodeTransport,
{
    pub fn new(gateway: G, transport: T, network: RemoteNetworkConfig) -> Self {
        Self {
            gateway,
            transport,
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
            if let Some(writer) = host
                .writer
                .lock()
                .expect("authority writer mutex should not be poisoned")
                .take()
            {
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
                    eprintln!("[session-sync] failed to handle authority command: {error}");
                }
            }
            GrpcAuthorityEvent::MirrorAccepted
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
            eprintln!("[session-sync] failed to extract session from target id `{target_id}`");
            LifecycleError::Protocol(format!(
                "failed to derive local session from target id `{target_id}`"
            ))
        })?;
        let socket_name = find_socket_for_session(&session_name).ok_or_else(|| {
            eprintln!(
                "[session-sync] no local socket owns session `{session_name}` for `{target_id}`"
            );
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
                transport_session_id: target_id
                    .splitn(3, ':')
                    .nth(2)
                    .unwrap_or(target_id)
                    .to_string(),
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
        let mut retried = false;

        loop {
            if !self.running_hosts.contains_key(&target_id) {
                self.ensure_authority_host(session_handle, &target_id)?;
            }

            let host = self.running_hosts.get_mut(&target_id).ok_or_else(|| {
                LifecycleError::Protocol("authority host cache lost entry".to_string())
            })?;
            if let Err(error) = send_command_to_host(host, command.clone()) {
                host.running.store(false, Ordering::Relaxed);
                if let Some(writer) = host
                    .writer
                    .lock()
                    .expect("authority writer mutex should not be poisoned")
                    .take()
                {
                    let _ = writer.shutdown(Shutdown::Both);
                }
                self.running_hosts.remove(&target_id);

                if retried {
                    eprintln!(
                        "[session-sync] authority host for `{target_id}` failed after retry: {error}"
                    );
                    return Err(error);
                }
                retried = true;
                eprintln!(
                    "[session-sync] authority host for `{target_id}` is stale; reconnecting..."
                );
                continue;
            }
            return Ok(());
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
