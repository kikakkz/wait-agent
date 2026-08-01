use crate::cli::{RemoteNetworkConfig, RemoteRuntimeOwnerCommand};
use crate::domain::session_catalog::{
    ManagedSessionAddress, ManagedSessionRecord, ManagedSessionTaskState, SessionAvailability,
    SessionTransport,
};
use crate::infra::error_log::ERROR_LOG;
use crate::lifecycle::LifecycleError;
use crate::runtime::remote_node::remote_node_ingress_server_runtime::{
    remote_node_ingress_owner_socket_path, start_owner_control_acceptor, OwnerLifecycleEvent,
    RemoteNodeIngressServerRuntime,
};
use crate::runtime::remote_node::remote_node_session_sync_runtime::{
    LocalCatalogChangeReason, LocalCatalogChangeRequest, RatatuiLocalAuthorityHostBackend,
    RatatuiLocalSessionCatalog, RatatuiLocalTargetExitObserver, RatatuiLocalTargetFactory,
    RemoteNodeSessionSyncRuntime,
};
use crate::runtime::remote_publication::ratatui_target_publication_backend::RatatuiRemoteTargetPublicationBackend;
use crate::runtime::remote_publication::remote_target_publication_runtime::RemoteTargetPublicationRuntime;
use crate::runtime::remote_runtime_owner_runtime::RemoteRuntimeOwnerRuntime;
use std::collections::HashMap;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use super::authority_host_io_loop::AuthorityHostIoLoop;
use super::authority_host_session::RatatuiAuthorityHostSession;
use super::client::ClientHandle;
use super::client_writer::ClientWriter;
use super::local_session::RatatuiLocalSession;
use super::remote_session::RatatuiRemoteSession;
use super::state_event::StateEvent;
use super::state_loop::StateEventLoop;

pub(crate) const DEFAULT_SESSION_ID: &str = "1";

/// Ratatui node server: holds all session state for a single `--port` and
/// survives TUI client disconnects.
///
/// One server process maps to one listener port. Multiple clients can attach to
/// the same server. Sessions inside the server are created via the TUI
/// (Ctrl+N/W/S), not via command-line arguments.
pub struct RatatuiNodeRuntime {
    network: RemoteNetworkConfig,
    shared: Arc<SharedState>,
    remote_owner: RemoteRuntimeOwnerRuntime,
}

/// Lock hierarchy for all `SharedState` fields:
///
/// 1. `sessions`
/// 2. `active_target`
/// 3. `local_sessions` / `remote_sessions` / `authority_host_sessions`
///
/// TUI client sockets are owned by `ClientWriter` and are not protected by any
/// `SharedState` lock.  `StateEventLoop` is the single writer of `SharedState`
/// and the only thread that decides when to broadcast snapshots.
pub(crate) struct SharedState {
    pub(crate) network: RemoteNetworkConfig,
    pub(crate) sessions: Mutex<HashMap<String, ManagedSessionRecord>>,
    pub(crate) active_target: Mutex<Option<String>>,
    pub(crate) client_count: AtomicUsize,
    pub(crate) clients: Arc<Mutex<Vec<ClientHandle>>>,
    pub(crate) start_time: Instant,
    pub(crate) shutdown: AtomicBool,
    pub(crate) local_sessions: Mutex<HashMap<String, Arc<RatatuiLocalSession>>>,
    pub(crate) authority_host_sessions: Mutex<HashMap<String, Arc<RatatuiAuthorityHostSession>>>,
    pub(crate) remote_sessions: Mutex<HashMap<String, Arc<RatatuiRemoteSession>>>,
    state_tx: Mutex<Option<mpsc::Sender<StateEvent>>>,
    authority_host_io_tx: Mutex<Option<super::authority_host_io_loop::AuthorityHostIoHandle>>,
    local_catalog_tx: Mutex<Option<mpsc::Sender<LocalCatalogChangeRequest>>>,
}

impl SharedState {
    pub(crate) fn new(network: RemoteNetworkConfig) -> Arc<Self> {
        Arc::new(Self {
            network,
            sessions: Mutex::new(HashMap::new()),
            active_target: Mutex::new(None),
            client_count: AtomicUsize::new(0),
            clients: Arc::new(Mutex::new(Vec::new())),
            start_time: Instant::now(),
            shutdown: AtomicBool::new(false),
            local_sessions: Mutex::new(HashMap::new()),
            authority_host_sessions: Mutex::new(HashMap::new()),
            remote_sessions: Mutex::new(HashMap::new()),
            state_tx: Mutex::new(None),
            authority_host_io_tx: Mutex::new(None),
            local_catalog_tx: Mutex::new(None),
        })
    }

    pub(crate) fn set_state_tx(&self, tx: mpsc::Sender<StateEvent>) {
        if let Ok(mut guard) = self.state_tx.lock() {
            *guard = Some(tx);
        }
    }

    pub(crate) fn state_sender(&self) -> mpsc::Sender<StateEvent> {
        self.state_tx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .cloned()
            .unwrap_or_else(|| {
                // Return a dangling sender so callers do not panic. Events sent
                // before the loop is started are simply dropped.
                let (tx, _) = mpsc::channel();
                tx
            })
    }

    pub(crate) fn set_authority_host_io_tx(
        &self,
        tx: super::authority_host_io_loop::AuthorityHostIoHandle,
    ) {
        if let Ok(mut guard) = self.authority_host_io_tx.lock() {
            *guard = Some(tx);
        }
    }

    pub(crate) fn authority_host_io_sender(
        &self,
    ) -> super::authority_host_io_loop::AuthorityHostIoHandle {
        self.authority_host_io_tx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .cloned()
            .unwrap_or_else(super::authority_host_io_loop::AuthorityHostIoHandle::dangling)
    }

    pub(crate) fn set_local_catalog_tx(&self, tx: mpsc::Sender<LocalCatalogChangeRequest>) {
        if let Ok(mut guard) = self.local_catalog_tx.lock() {
            *guard = Some(tx);
        }
    }

    pub(crate) fn notify_local_catalog_changed(&self, reason: LocalCatalogChangeReason) {
        if let Ok(guard) = self.local_catalog_tx.lock() {
            if let Some(tx) = guard.as_ref() {
                let request = LocalCatalogChangeRequest {
                    reason,
                    ack_tx: None,
                };
                let _ = tx.send(request);
            }
        }
    }

    pub(super) fn detach_all_clients(&self) {
        // Mark every known client as removed.  The actual sockets are owned by
        // `ClientWriter`; callers must also send `Unregister` requests for each
        // client id.
        let mut guard = self.clients.lock().unwrap_or_else(|e| e.into_inner());
        for handle in guard.drain(..) {
            handle.removed.store(true, Ordering::SeqCst);
        }
        drop(guard);
        self.client_count.store(0, Ordering::SeqCst);
    }

    /// Mark a session as exited, remove its runtime, switch to the next
    /// available session, and shut down the server when the last session exits.
    pub(super) fn handle_session_exit(&self, target_id: &str) {
        let record = {
            let mut guard = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            guard.remove(target_id)
        };
        let was_local = record
            .as_ref()
            .map(|r| r.address.transport() == &SessionTransport::Local)
            .unwrap_or(false);

        if was_local {
            let mut local_guard = self
                .local_sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            local_guard.remove(target_id);
            drop(local_guard);

            // Authority-host sessions are keyed by their short session id.
            if let Some(session_id) = target_id.rsplit_once(':').map(|(_, id)| id) {
                let mut host_guard = self
                    .authority_host_sessions
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                host_guard.remove(session_id);
            }
        } else {
            let session = {
                let remote_guard = self
                    .remote_sessions
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                remote_guard.get(target_id).cloned()
            };
            if let Some(session) = session {
                session.stop();
            }
            let mut remote_guard = self
                .remote_sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            remote_guard.remove(target_id);
            drop(remote_guard);
        }

        // Server lifetime is tied to local sessions. Remote sessions are views
        // into other hosts and should not keep this server alive when the last
        // local session exits. On restart, clients are expected to reconnect.
        let remaining_local: Vec<String> = {
            let guard = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            guard
                .values()
                .filter(|r| {
                    r.address.transport() == &SessionTransport::Local
                        && r.availability == SessionAvailability::Online
                })
                .map(|r| r.address.qualified_target())
                .collect()
        };

        {
            let mut active_guard = self.active_target.lock().unwrap_or_else(|e| e.into_inner());
            if remaining_local.is_empty() {
                // Peer node servers (started with --connect) are long-lived:
                // they only host sessions on behalf of remote viewers and must
                // stay alive so future sessions can be created.  The local node
                // server (no --connect) exits when its last local session ends.
                if self.network.connect.is_none() {
                    ERROR_LOG.log(format!(
                        "[ratatui-node] last local session {target_id} exited; shutting down"
                    ));
                    self.shutdown.store(true, Ordering::SeqCst);
                    let _ =
                        UnixStream::connect(super::socket::ratatui_socket_path(self.network.port));
                    *active_guard = None;
                } else {
                    ERROR_LOG.log(format!(
                        "[ratatui-node] peer node session {target_id} exited; keeping server alive"
                    ));
                    *active_guard = None;
                }
            } else {
                // Pick the next local session. Prefer one that is not the
                // just-exited session, falling back to the first remaining one.
                let next = remaining_local
                    .iter()
                    .find(|t| *t != target_id)
                    .or(remaining_local.first())
                    .cloned()
                    .unwrap_or_default();
                *active_guard = Some(next);
            }
        }

        self.notify_local_catalog_changed(LocalCatalogChangeReason::LocalTargetExited {
            target_session_name: target_id.to_string(),
        });
    }

    /// Update the displayed command name from the terminal title.
    pub(super) fn set_local_session_title(&self, target_id: &str, title: String) {
        let mut guard = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(record) = guard.get_mut(target_id) {
            record.display_command_name = Some(title);
        }
    }

    /// Return the authority id used for local sessions hosted by this server.
    pub(crate) fn local_authority_id(&self) -> String {
        format!("local#{}", self.network.port)
    }

    /// Spawn a real local PTY session and register it in the catalog.
    /// Called once during server startup before `StateEventLoop` is running.
    /// This is the only allowed direct mutation of `SharedState` outside the
    /// single-writer loop; all later session creation must go through
    /// `StateEvent::CreateAuthorityHostSession` or `ClientCommand::CreateLocalSession`.
    pub(super) fn create_local_session(
        self: &Arc<Self>,
        session_id: &str,
        cols: u16,
        rows: u16,
    ) -> Result<String, LifecycleError> {
        let command_name = std::env::var("SHELL")
            .ok()
            .and_then(|s| s.rsplit_once('/').map(|(_, name)| name.to_string()))
            .unwrap_or_else(|| "bash".to_string());

        let target_id = {
            let authority_id = self.local_authority_id();
            ManagedSessionAddress::local(&authority_id, session_id).qualified_target()
        };

        let session = RatatuiLocalSession::spawn(
            target_id.clone(),
            command_name.clone(),
            cols,
            rows,
            self.clone(),
        )?;

        {
            let mut local_guard = self
                .local_sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            local_guard.insert(target_id.clone(), session);
        }

        {
            let mut guard = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            let record = ManagedSessionRecord {
                address: ManagedSessionAddress::local(self.local_authority_id(), session_id),
                selector: None,
                availability: SessionAvailability::Online,
                workspace_dir: None,
                workspace_key: None,
                session_role: None,
                opened_by: Vec::new(),
                attached_clients: 0,
                window_count: 1,
                command_name: Some(command_name),
                display_command_name: None,
                current_path: None,
                task_state: ManagedSessionTaskState::Input,
            };
            guard.insert(target_id.clone(), record);
        }

        *self.active_target.lock().unwrap_or_else(|e| e.into_inner()) = Some(target_id.clone());
        self.notify_local_catalog_changed(LocalCatalogChangeReason::LocalRuntimeChanged);
        Ok(target_id)
    }

    /// Create an authority-host session (a raw PTY session for a remote viewer)
    /// and register it in the catalog.
    ///
    /// The caller is responsible for registering the PTY master fd with
    /// `AuthorityHostIoLoop`.
    pub(super) fn create_authority_host_session(
        self: &Arc<Self>,
        cols: u16,
        rows: u16,
    ) -> Result<(String, RatatuiAuthorityHostSession, String), LifecycleError> {
        let authority_id = self.local_authority_id();
        let id = {
            let guard = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            format!("{}", guard.len() + 1)
        };
        let target_id = ManagedSessionAddress::local(&authority_id, &id).qualified_target();

        let command_name = std::env::var("SHELL")
            .ok()
            .and_then(|s| s.rsplit_once('/').map(|(_, name)| name.to_string()))
            .unwrap_or_else(|| "bash".to_string());

        let session =
            RatatuiAuthorityHostSession::spawn(id.clone(), command_name.clone(), cols, rows)?;

        {
            let mut guard = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            let record = ManagedSessionRecord {
                address: ManagedSessionAddress::local(authority_id, &id),
                selector: None,
                availability: SessionAvailability::Online,
                workspace_dir: None,
                workspace_key: None,
                session_role: None,
                opened_by: Vec::new(),
                attached_clients: 0,
                window_count: 1,
                command_name: Some(command_name),
                display_command_name: None,
                current_path: None,
                task_state: ManagedSessionTaskState::Running,
            };
            guard.insert(target_id.clone(), record);
        }

        *self.active_target.lock().unwrap_or_else(|e| e.into_inner()) = Some(target_id.clone());

        // The caller (StateEventLoop) is responsible for registering the PTY
        // master fd with `AuthorityHostIoLoop` and then inserting the session
        // into `authority_host_sessions`.
        Ok((id, session, target_id))
    }

    /// Resize the active local session, if any.
    pub(crate) fn resize_active_local_session(&self, cols: u16, rows: u16) {
        let active = self
            .active_target
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(target) = active {
            let guard = self
                .local_sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(session) = guard.get(&target) {
                session.resize(cols, rows);
            }
        }
    }

    /// Return the workspace id used for authority transport socket naming.
    pub(crate) fn workspace_id(&self) -> String {
        format!("ratatui-{}", self.network.port)
    }

    /// Open or reuse a remote session viewer for the given record.
    pub(crate) fn ensure_remote_session(
        self: &Arc<Self>,
        record: &ManagedSessionRecord,
    ) -> Result<String, LifecycleError> {
        let target_id = record.address.qualified_target();
        {
            let guard = self
                .remote_sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(session) = guard.get(&target_id) {
                session.send_open_mirror(80, 24);
                return Ok(target_id);
            }
        }
        let session =
            RatatuiRemoteSession::open(record, &self.workspace_id(), &self.network, self)?;
        session.send_open_mirror(80, 24);
        {
            let mut guard = self
                .remote_sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            guard.insert(target_id.clone(), session);
        }
        Ok(target_id)
    }

    /// Forward input bytes to a specific remote session keyed by qualified target.
    pub(crate) fn feed_remote_session_input(&self, target_id: &str, bytes: impl Into<Vec<u8>>) {
        let session = {
            let guard = self
                .remote_sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            guard.get(target_id).cloned()
        };
        if let Some(session) = session {
            session.feed_input(bytes);
        }
    }

    /// Remove all remote-peer sessions whose authority id matches the given
    /// node.  Called when a remote peer reconnects with a new node instance,
    /// which means any previous sessions hosted by that peer are stale.
    pub(crate) fn remove_remote_sessions_for_node(&self, node_id: &str) {
        let to_remove: Vec<String> = {
            let guard = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            guard
                .values()
                .filter(|record| {
                    *record.address.transport() == SessionTransport::RemotePeer
                        && record.address.authority_id() == node_id
                })
                .map(|record| record.address.qualified_target())
                .collect()
        };
        for target_id in to_remove {
            self.handle_session_exit(&target_id);
        }
    }

    /// Resize the active remote session, if any.
    pub(crate) fn resize_active_remote_session(&self, cols: u16, rows: u16) {
        let active = self
            .active_target
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let Some(target) = active else {
            return;
        };
        let guard = self
            .remote_sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(session) = guard.get(&target) {
            session.resize(cols, rows);
            session.resize_local_screen(cols, rows);
        }
    }

    /// Return the full scrollback history plus visible screen for a target.
    pub(crate) fn history_for_target(&self, target_id: &str) -> Option<(Vec<String>, Vec<String>)> {
        let guard = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        let transport = guard.get(target_id).map(|s| s.address.transport().clone());
        drop(guard);
        match transport {
            Some(SessionTransport::RemotePeer) => {
                let guard = self
                    .remote_sessions
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                guard.get(target_id).map(|s| s.history_snapshot())
            }
            Some(SessionTransport::Local) => {
                let guard = self
                    .local_sessions
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                guard.get(target_id).map(|s| s.history_snapshot())
            }
            None => None,
        }
    }
}

impl RatatuiNodeRuntime {
    pub fn from_network(network: RemoteNetworkConfig) -> Result<Self, LifecycleError> {
        let remote_owner = RemoteRuntimeOwnerRuntime::from_build_env_with_network(network.clone())?;

        let shared = SharedState::new(network.clone());

        // Create the default local session with a reasonable initial size.
        // The client will send a RESIZE once it knows the terminal dimensions.
        // Peer node servers (with --connect) should not create a default local
        // session; they only host sessions requested by remote viewers.
        if network.connect.is_none() {
            let _ = shared.create_local_session(DEFAULT_SESSION_ID, 80, 24);
        }

        Ok(Self {
            network: network.clone(),
            shared,
            remote_owner,
        })
    }

    pub fn run(&self) -> Result<(), LifecycleError> {
        let socket_path = super::socket::ratatui_socket_path(self.network.port);
        ERROR_LOG.log(format!(
            "[ratatui-node] starting port={} socket={}",
            self.network.port,
            socket_path.display()
        ));

        if let Some(parent) = socket_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        // Remove stale socket before binding.
        let _ = std::fs::remove_file(&socket_path);

        let listener = UnixListener::bind(&socket_path).map_err(|error| {
            LifecycleError::Io(
                format!(
                    "failed to bind ratatui node socket {}",
                    socket_path.display()
                ),
                error,
            )
        })?;

        ERROR_LOG.log("[ratatui-node] listening".to_string());

        // Start the event-driven IO loops before any session can be created.
        // AuthorityHostIoLoop uses SharedState::state_sender() lazily so it
        // picks up the real sender once set_state_tx() is called below.
        let (catalog_tx, catalog_rx) = std::sync::mpsc::channel::<LocalCatalogChangeRequest>();
        let authority_host_io = AuthorityHostIoLoop::start(self.shared.clone())?;
        let client_writer = ClientWriter::start();
        let state_event_loop = StateEventLoop::start(
            self.shared.clone(),
            catalog_tx.clone(),
            &authority_host_io,
            client_writer.clone(),
        )?;
        self.shared.set_state_tx(state_event_loop.sender());
        self.shared
            .set_authority_host_io_tx(authority_host_io.sender());
        self.shared.set_local_catalog_tx(catalog_tx);

        // Start the remote runtime owner inside the server process so that
        // discovered remote sessions are kept in-memory and shared with the
        // remote target publication / sync runtimes.
        let owner_network = self.network.clone();
        let owner = self.remote_owner.clone();
        let _owner_thread = std::thread::spawn(move || {
            if let Err(error) = owner.run_owner(RemoteRuntimeOwnerCommand::default()) {
                ERROR_LOG.log(format!(
                    "[ratatui-node] remote runtime owner exited: {error}"
                ));
            }
        });

        // Start the remote session sync runtime when a connect endpoint is
        // configured. It publishes the local ratatui session catalog to the
        // remote authority and accepts create-session requests from it.
        let _sync_guard = if self.network.connect.is_some() {
            let sync_network = self.network.clone();
            let shared = self.shared.clone();
            let backend =
                RatatuiRemoteTargetPublicationBackend::new(shared.clone(), sync_network.clone());
            let publication_runtime = RemoteTargetPublicationRuntime::with_network_and_backend(
                sync_network.clone(),
                backend,
            )?;
            let sync_runtime = RemoteNodeSessionSyncRuntime::new_with_backends(
                RatatuiLocalSessionCatalog::new(shared.clone()),
                crate::infra::remote_grpc_transport::GrpcRemoteNodeTransport::new(),
                RatatuiLocalTargetExitObserver,
                RatatuiLocalTargetFactory::new(shared.clone(), sync_network.clone()),
                RatatuiLocalAuthorityHostBackend::new(shared.clone(), sync_network.clone()),
                Some(publication_runtime),
                sync_network,
            );
            match sync_runtime.start_with_local_catalog_changes(catalog_rx) {
                Ok(guard) => Some(guard),
                Err(error) => {
                    ERROR_LOG.log(format!(
                        "[ratatui-node] failed to start remote session sync: {error}"
                    ));
                    None
                }
            }
        } else {
            None
        };

        // Start the remote node ingress server inside the server process so
        // peers can connect in and request local target sessions.
        let ingress_network = self.network.clone();
        let shared = self.shared.clone();
        let ingress_backend =
            RatatuiRemoteTargetPublicationBackend::new(shared.clone(), ingress_network.clone());
        let ingress_publication_runtime = RemoteTargetPublicationRuntime::with_network_and_backend(
            ingress_network.clone(),
            ingress_backend,
        )?;
        let ingress_runtime = RemoteNodeIngressServerRuntime::new_with_backends(
            ingress_network,
            ingress_publication_runtime,
            RatatuiLocalTargetFactory::new(shared.clone(), self.network.clone()),
            RatatuiLocalAuthorityHostBackend::new(shared.clone(), self.network.clone()),
        );
        let _ingress_guard = match ingress_runtime.start() {
            Ok(guard) => {
                // Bind the same local control socket that tmux-sidecar ingress
                // owners use, so __remote-session-creation and Ctrl+W connect
                // can reach this single-process server.
                let owner_socket_path = remote_node_ingress_owner_socket_path(&self.network);
                let _ = std::fs::remove_file(&owner_socket_path);
                if let Ok(owner_listener) =
                    std::os::unix::net::UnixListener::bind(&owner_socket_path)
                {
                    if let Some(owner_tx) = guard.owner_event_sender() {
                        let (_lifecycle_tx, _lifecycle_rx) =
                            std::sync::mpsc::channel::<OwnerLifecycleEvent>();
                        let _owner_acceptor =
                            start_owner_control_acceptor(owner_listener, &owner_tx, _lifecycle_tx);
                    }
                }
                Some(guard)
            }
            Err(error) => {
                ERROR_LOG.log(format!(
                    "[ratatui-node] failed to start remote node ingress: {error}"
                ));
                None
            }
        };

        let clients = self.shared.clients.clone();
        let client_writer_for_accept = client_writer.clone();

        for stream in listener.incoming() {
            if self.shared.shutdown.load(Ordering::SeqCst) {
                break;
            }

            match stream {
                Ok(stream) => {
                    let clients = clients.clone();
                    let shared = self.shared.clone();
                    let client_writer = client_writer_for_accept.clone();
                    std::thread::spawn(move || {
                        let client_id =
                            super::client::NEXT_CLIENT_ID.fetch_add(1, Ordering::SeqCst);
                        if let Err(error) = super::client::handle_client(
                            stream,
                            client_id,
                            clients,
                            shared,
                            client_writer,
                        ) {
                            ERROR_LOG
                                .log(format!("[ratatui-node] client handler error: {error:?}"));
                        }
                    });
                }
                Err(error) => {
                    ERROR_LOG.log(format!("[ratatui-node] accept error: {error:?}"));
                }
            }
        }

        let _ = std::fs::remove_file(&socket_path);
        if let Err(error) = RemoteRuntimeOwnerRuntime::shutdown_owner(&owner_network) {
            ERROR_LOG.log(format!(
                "[ratatui-node] remote runtime owner shutdown error: {error}"
            ));
        }
        ERROR_LOG.log(format!(
            "[ratatui-node] shutting down port={}",
            self.network.port
        ));
        Ok(())
    }
}
