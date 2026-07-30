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
/// 1. `clients` (outermost, only held during `broadcast_snapshot`)
/// 2. `sessions`
/// 3. `active_target`
/// 4. `local_sessions` / `remote_sessions` / `authority_host_sessions`
///
/// `StateEventLoop` is the single writer of `SharedState` and the only loop
/// that calls `broadcast_snapshot`.  Other threads send events to trigger
/// snapshot broadcasts instead of calling `broadcast_snapshot` directly.
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
    authority_host_io_tx:
        Mutex<Option<mpsc::Sender<super::authority_host_io_loop::AuthorityHostIoRequest>>>,
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
        tx: mpsc::Sender<super::authority_host_io_loop::AuthorityHostIoRequest>,
    ) {
        if let Ok(mut guard) = self.authority_host_io_tx.lock() {
            *guard = Some(tx);
        }
    }

    pub(crate) fn authority_host_io_sender(
        &self,
    ) -> mpsc::Sender<super::authority_host_io_loop::AuthorityHostIoRequest> {
        self.authority_host_io_tx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .cloned()
            .unwrap_or_else(|| {
                let (tx, _) = mpsc::channel();
                tx
            })
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

    pub(crate) fn detach_all_clients(&self) {
        use std::net::Shutdown;
        let mut guard = self.clients.lock().unwrap_or_else(|e| e.into_inner());
        for handle in guard.drain(..) {
            handle.removed.store(true, Ordering::SeqCst);
            let _ = handle.stream.shutdown(Shutdown::Both);
        }
        drop(guard);
        self.client_count.store(0, Ordering::SeqCst);
    }

    pub(crate) fn broadcast_snapshot(&self) -> Result<(), LifecycleError> {
        super::snapshot::broadcast_snapshot(&self.clients, self)
    }

    /// Mark a session as exited, remove its runtime, switch to the next
    /// available session, and shut down the server when the last session exits.
    pub(crate) fn handle_session_exit(&self, session_id: &str) {
        let (was_local, qualified_target) = {
            let mut guard = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            let record = guard.remove(session_id);
            let was_local = record
                .as_ref()
                .map(|r| r.address.transport() == &SessionTransport::LocalTmux)
                .unwrap_or(false);
            let qualified_target = record.as_ref().map(|r| r.address.qualified_target());
            drop(guard);

            if was_local {
                let mut local_guard = self
                    .local_sessions
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                local_guard.remove(session_id);
                drop(local_guard);
            } else {
                let mut remote_guard = self
                    .remote_sessions
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                if let Some(target) = qualified_target.as_deref() {
                    remote_guard.remove(target);
                }
                drop(remote_guard);
            }

            {
                let mut host_guard = self
                    .authority_host_sessions
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                host_guard.remove(session_id);
            }

            (was_local, qualified_target)
        };

        let remaining: Vec<String> = {
            let guard = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            guard
                .values()
                .filter(|r| r.availability == SessionAvailability::Online)
                .map(|r| r.address.qualified_target())
                .collect()
        };

        {
            let mut active_guard = self.active_target.lock().unwrap_or_else(|e| e.into_inner());
            if remaining.is_empty() {
                // Only the local TUI server shuts down when its last session exits.
                // Remote peer nodes stay alive so they can publish the exit event
                // back to the authority and accept new sessions later.
                if was_local && self.network.connect.is_none() {
                    ERROR_LOG.log(format!(
                        "[ratatui-node] last local session {session_id} exited; shutting down"
                    ));
                    self.shutdown.store(true, Ordering::SeqCst);
                    let _ =
                        UnixStream::connect(super::socket::ratatui_socket_path(self.network.port));
                }
                *active_guard = None;
            } else {
                // Pick the next session. Prefer one that is not the just-exited
                // session, falling back to the first remaining session.
                let next = remaining
                    .iter()
                    .find(|t| Some(t.as_str()) != qualified_target.as_deref())
                    .or(remaining.first())
                    .cloned()
                    .unwrap_or_default();
                *active_guard = Some(next);
            }
        }

        let _ = self.broadcast_snapshot();
        self.notify_local_catalog_changed(LocalCatalogChangeReason::LocalTargetExited {
            target_session_name: session_id.to_string(),
        });
    }

    /// Update the displayed command name from the terminal title.
    pub(crate) fn set_local_session_title(&self, session_id: &str, title: String) {
        let mut guard = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(record) = guard.get_mut(session_id) {
            record.display_command_name = Some(title);
        }
    }

    /// Spawn a real local PTY session and register it in the catalog.
    pub(crate) fn create_local_session(
        self: &Arc<Self>,
        session_id: &str,
        cols: u16,
        rows: u16,
    ) -> Result<String, LifecycleError> {
        let command_name = std::env::var("SHELL")
            .ok()
            .and_then(|s| s.rsplit_once('/').map(|(_, name)| name.to_string()))
            .unwrap_or_else(|| "bash".to_string());

        let session = RatatuiLocalSession::spawn(
            session_id.to_string(),
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
            local_guard.insert(session_id.to_string(), session);
        }

        let target = {
            let mut guard = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            let record = ManagedSessionRecord {
                address: ManagedSessionAddress::local_tmux(
                    self.network.port.to_string(),
                    session_id,
                ),
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
            let target = record.address.qualified_target();
            guard.insert(session_id.to_string(), record);
            target
        };

        *self.active_target.lock().unwrap_or_else(|e| e.into_inner()) = Some(target.clone());
        self.notify_local_catalog_changed(LocalCatalogChangeReason::LocalRuntimeChanged);
        Ok(target)
    }

    /// Create an authority-host session (a raw PTY session for a remote viewer)
    /// and register it in the catalog.
    ///
    /// The caller is responsible for registering the PTY master fd with
    /// `AuthorityHostIoLoop`.
    pub(crate) fn create_authority_host_session(
        self: &Arc<Self>,
        cols: u16,
        rows: u16,
    ) -> Result<(String, RatatuiAuthorityHostSession, String), LifecycleError> {
        let id = {
            let guard = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            format!("{}", guard.len() + 1)
        };

        let command_name = std::env::var("SHELL")
            .ok()
            .and_then(|s| s.rsplit_once('/').map(|(_, name)| name.to_string()))
            .unwrap_or_else(|| "bash".to_string());

        let session =
            RatatuiAuthorityHostSession::spawn(id.clone(), command_name.clone(), cols, rows)?;

        let target = {
            let mut guard = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            let record = ManagedSessionRecord {
                address: ManagedSessionAddress::local_tmux(self.network.port.to_string(), &id),
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
            let target = record.address.qualified_target();
            guard.insert(id.clone(), record);
            target
        };

        *self.active_target.lock().unwrap_or_else(|e| e.into_inner()) = Some(target.clone());

        // The caller (StateEventLoop) is responsible for registering the PTY
        // master fd with `AuthorityHostIoLoop` and then inserting the session
        // into `authority_host_sessions`.
        Ok((id, session, target))
    }

    /// Resize the active local session, if any.
    pub(crate) fn resize_active_local_session(&self, cols: u16, rows: u16) {
        let active = self
            .active_target
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let session_id = active
            .as_deref()
            .and_then(|target| target.split_once(':').map(|(_, id)| id.to_string()));
        if let Some(id) = session_id {
            let guard = self
                .local_sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(session) = guard.get(&id) {
                session.resize(cols, rows);
            }
        }
    }

    /// Forward input bytes to a specific local session.
    pub(crate) fn feed_local_session_input(&self, session_id: &str, bytes: impl Into<Vec<u8>>) {
        let guard = self
            .local_sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(session) = guard.get(session_id) {
            session.feed_input(bytes);
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

    /// Forward input bytes to the active remote session, if any.
    ///
    /// The `active_target` and `remote_sessions` locks are only held long enough
    /// to clone the session handle; the actual write happens without them so
    /// that `RatatuiRemoteSession::feed_input` cannot create a lock-order cycle
    /// with `broadcast_snapshot`.
    pub(crate) fn feed_active_remote_session_input(&self, bytes: impl Into<Vec<u8>>) {
        let session = {
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
            guard.get(&target).cloned()
        };
        if let Some(session) = session {
            session.feed_input(bytes);
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
}

impl RatatuiNodeRuntime {
    pub fn from_network(network: RemoteNetworkConfig) -> Result<Self, LifecycleError> {
        let remote_owner = RemoteRuntimeOwnerRuntime::from_build_env_with_network(network.clone())?;

        let shared = SharedState::new(network.clone());

        // Create the default local session with a reasonable initial size.
        // The client will send a RESIZE once it knows the terminal dimensions.
        let _ = shared.create_local_session(DEFAULT_SESSION_ID, 80, 24);

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
        let state_event_loop =
            StateEventLoop::start(self.shared.clone(), catalog_tx.clone(), &authority_host_io)?;
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

        for stream in listener.incoming() {
            if self.shared.shutdown.load(Ordering::SeqCst) {
                break;
            }

            match stream {
                Ok(stream) => {
                    let clients = clients.clone();
                    let shared = self.shared.clone();
                    std::thread::spawn(move || {
                        let client_id =
                            super::client::NEXT_CLIENT_ID.fetch_add(1, Ordering::SeqCst);
                        if let Err(error) =
                            super::client::handle_client(stream, client_id, clients, shared)
                        {
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
