use crate::cli::{RemoteNetworkConfig, RemoteRuntimeOwnerCommand};
use crate::domain::session_catalog::{
    ManagedSessionAddress, ManagedSessionRecord, ManagedSessionTaskState, SessionAvailability,
};
use crate::infra::error_log::ERROR_LOG;
use crate::lifecycle::LifecycleError;
use crate::runtime::remote_node::remote_node_ingress_server_runtime::{
    remote_node_ingress_owner_socket_path, start_owner_control_acceptor, OwnerLifecycleEvent,
    RemoteNodeIngressServerRuntime,
};
use crate::runtime::remote_node::remote_node_session_sync_runtime::{
    RatatuiLocalAuthorityHostBackend, RatatuiLocalSessionCatalog, RatatuiLocalTargetExitObserver,
    RatatuiLocalTargetFactory, RemoteNodeSessionSyncRuntime,
};
use crate::runtime::remote_publication::ratatui_target_publication_backend::RatatuiRemoteTargetPublicationBackend;
use crate::runtime::remote_publication::remote_target_publication_runtime::RemoteTargetPublicationRuntime;
use crate::runtime::remote_runtime_owner_runtime::RemoteRuntimeOwnerRuntime;
use std::collections::HashMap;
use std::os::unix::net::UnixListener;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use super::authority_host_session::RatatuiAuthorityHostSession;
use super::client::ClientHandle;
use super::local_session::{LocalSessionEvent, RatatuiLocalSession};
use super::remote_session::RatatuiRemoteSession;

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
    event_tx: Mutex<mpsc::Sender<LocalSessionEvent>>,
}

impl SharedState {
    pub(crate) fn new(network: RemoteNetworkConfig) -> Arc<Self> {
        let (event_tx, event_rx) = mpsc::channel::<LocalSessionEvent>();

        let shared = Arc::new(Self {
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
            event_tx: Mutex::new(event_tx),
        });

        // Dedicated worker that applies local-session events to shared state.
        // Running on a separate thread guarantees we never hold `Term`'s lock
        // while locking `sessions` or `local_sessions`, which prevents deadlock.
        let weak = Arc::downgrade(&shared);
        std::thread::spawn(move || loop {
            let Ok(event) = event_rx.recv() else {
                break;
            };
            let Some(shared) = weak.upgrade() else {
                break;
            };

            match event {
                LocalSessionEvent::Wakeup => {
                    let _ = shared.broadcast_snapshot();
                }
                LocalSessionEvent::ChildExit { session_id, status } => {
                    ERROR_LOG.log(format!(
                            "[ratatui-local-session] child exited session={session_id} status={status:?}"
                        ));
                    shared.mark_local_session_exited(&session_id);
                    let _ = shared.broadcast_snapshot();
                }
                LocalSessionEvent::Exit { session_id } => {
                    shared.mark_local_session_exited(&session_id);
                    let _ = shared.broadcast_snapshot();
                }
                LocalSessionEvent::Title { session_id, title } => {
                    shared.set_local_session_title(&session_id, title);
                    let _ = shared.broadcast_snapshot();
                }
            }
        });

        shared
    }

    pub(crate) fn send_local_session_event(&self, event: LocalSessionEvent) {
        if let Ok(tx) = self.event_tx.lock() {
            let _ = tx.send(event);
        }
    }

    pub(crate) fn broadcast_snapshot(&self) -> Result<(), LifecycleError> {
        super::snapshot::broadcast_snapshot(&self.clients, self)
    }

    /// Mark a local session as exited in the session catalog.
    pub(crate) fn mark_local_session_exited(&self, session_id: &str) {
        let mut guard = self.sessions.lock().unwrap();
        if let Some(record) = guard.get_mut(session_id) {
            record.availability = SessionAvailability::Exited;
            record.task_state = ManagedSessionTaskState::Unknown;
        }
        drop(guard);

        let mut local_guard = self.local_sessions.lock().unwrap();
        local_guard.remove(session_id);
        drop(local_guard);
    }

    /// Update the displayed command name from the terminal title.
    pub(crate) fn set_local_session_title(&self, session_id: &str, title: String) {
        let mut guard = self.sessions.lock().unwrap();
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
            let mut local_guard = self.local_sessions.lock().unwrap();
            local_guard.insert(session_id.to_string(), session);
        }

        let target = {
            let mut guard = self.sessions.lock().unwrap();
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

        *self.active_target.lock().unwrap() = Some(target.clone());
        self.send_local_session_event(LocalSessionEvent::Wakeup);
        Ok(target)
    }

    /// Resize the active local session, if any.
    pub(crate) fn resize_active_local_session(&self, cols: u16, rows: u16) {
        let active = self.active_target.lock().unwrap().clone();
        let session_id = active
            .as_deref()
            .and_then(|target| target.split_once(':').map(|(_, id)| id.to_string()));
        if let Some(id) = session_id {
            let guard = self.local_sessions.lock().unwrap();
            if let Some(session) = guard.get(&id) {
                session.resize(cols, rows);
            }
        }
    }

    /// Forward input bytes to a specific local session.
    pub(crate) fn feed_local_session_input(&self, session_id: &str, bytes: Vec<u8>) {
        let guard = self.local_sessions.lock().unwrap();
        if let Some(session) = guard.get(session_id) {
            session.feed_input(bytes);
        }
    }

    /// Forward input bytes to an authority-host session.
    pub(crate) fn feed_authority_host_session_input(&self, session_id: &str, bytes: Vec<u8>) {
        let guard = self.authority_host_sessions.lock().unwrap();
        if let Some(session) = guard.get(session_id) {
            session.feed_input(bytes);
        }
    }

    /// Resize an authority-host session.
    pub(crate) fn resize_authority_host_session(&self, session_id: &str, cols: u16, rows: u16) {
        let guard = self.authority_host_sessions.lock().unwrap();
        if let Some(session) = guard.get(session_id) {
            session.resize(cols, rows);
        }
    }

    /// Remove an authority-host session from the catalog.
    pub(crate) fn remove_authority_host_session(&self, session_id: &str) {
        let mut guard = self.authority_host_sessions.lock().unwrap();
        guard.remove(session_id);
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
            let guard = self.remote_sessions.lock().unwrap();
            if let Some(session) = guard.get(&target_id) {
                session.send_open_mirror(80, 24);
                return Ok(target_id);
            }
        }
        let session = RatatuiRemoteSession::open(record, &self.workspace_id(), &self.network)?;
        session.send_open_mirror(80, 24);
        {
            let mut guard = self.remote_sessions.lock().unwrap();
            guard.insert(target_id.clone(), session);
        }
        Ok(target_id)
    }

    /// Forward input bytes to the active remote session, if any.
    pub(crate) fn feed_active_remote_session_input(&self, bytes: Vec<u8>) {
        let active = self.active_target.lock().unwrap().clone();
        let Some(target) = active else {
            return;
        };
        let guard = self.remote_sessions.lock().unwrap();
        if let Some(session) = guard.get(&target) {
            session.feed_input(bytes);
        }
    }

    /// Resize the active remote session, if any.
    pub(crate) fn resize_active_remote_session(&self, cols: u16, rows: u16) {
        let active = self.active_target.lock().unwrap().clone();
        let Some(target) = active else {
            return;
        };
        let guard = self.remote_sessions.lock().unwrap();
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
            let (_catalog_tx, catalog_rx) = std::sync::mpsc::channel();
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
