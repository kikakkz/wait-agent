use crate::cli::{RemoteNetworkConfig, RemoteRuntimeOwnerCommand};
use crate::domain::session_catalog::{
    ManagedSessionAddress, ManagedSessionRecord, ManagedSessionTaskState, SessionAvailability,
    SessionTransport,
};
use crate::infra::error_log::ERROR_LOG;
use crate::infra::remote_node_paths::remote_node_ingress_owner_socket_path;
use crate::lifecycle::LifecycleError;
use crate::ports::hooks_config::HooksConfigPort;
use crate::ports::session_creation::SessionCreationPort;
use crate::ports::target_registry::TargetRegistryPort;
use crate::remote::node::remote_node_ingress_server_runtime::{
    start_owner_control_acceptor, InternalEvent, OwnerLifecycleEvent,
    RemoteNodeIngressServerRuntime,
};
use crate::remote::node::remote_node_session_sync_runtime::{
    LocalCatalogChangeReason, LocalCatalogChangeRequest, RatatuiLocalAuthorityHostBackend,
    RatatuiLocalSessionCatalog, RatatuiLocalTargetExitObserver, RatatuiLocalTargetFactory,
    RemoteNodeSessionSyncRuntime,
};
use crate::remote::node::remote_runtime_owner_runtime::RemoteRuntimeOwnerRuntime;
use crate::remote::publication::ratatui_target_publication_backend::RatatuiRemoteTargetPublicationBackend;
use crate::remote::publication::remote_target_publication_runtime::RemoteTargetPublicationRuntime;
use std::collections::HashMap;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use super::agent_signal_env::AgentSignalEnv;
use super::agent_signal_server::AgentSignalServer;
use super::authority_host_io_loop::AuthorityHostIoLoop;
use super::authority_host_session::RatatuiAuthorityHostSession;
use super::client::ClientHandle;
use super::client_writer::ClientWriter;
use super::local_session::RatatuiLocalSession;
use super::network_probe::NetworkProbe;
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
    hooks_config_ports: Vec<Box<dyn HooksConfigPort>>,
    settings_store: crate::infra::settings_store::SettingsStore,
}

/// Lock hierarchy for all `SharedState` fields:
///
/// 1. `sessions.sessions`
/// 2. `sessions.active_target`
/// 3. `sessions.local_sessions` / `sessions.remote_sessions` / `sessions.authority_host_sessions`
///
/// Cross-thread code must acquire locks in this order. `StateEventLoop` is the
/// single writer of `SharedState`; paths inside the loop that hold both
/// `sessions.sessions` and `sessions.active_target` run on that single thread
/// and therefore cannot deadlock with each other.
///
/// TUI client sockets are owned by `ClientWriter` and are not protected by any
/// `SharedState` lock. `StateEventLoop` is the only thread that decides when to
/// broadcast snapshots.
/// Direction in which the gRPC node session for a remote peer was established.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum RemoteNodeConnectionMode {
    /// The control host dials the remote peer's listening waitagent.
    OutboundDial,
    /// The remote peer dials the control host via `--connect`.
    InboundConnect,
}

/// Metadata needed to reconnect to a remote peer without re-bootstrapping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemoteNodeConnectionInfo {
    pub mode: RemoteNodeConnectionMode,
    pub host: String,
    pub port: u16,
    pub tls_pin_sha256: String,
    pub profile_name: String,
}

pub(crate) struct SharedState {
    pub(crate) network: RemoteNetworkConfig,
    pub(crate) public_endpoint_override: Mutex<Option<String>>,
    pub(crate) start_time: Instant,
    pub(crate) shutdown: AtomicBool,
    pub(crate) agent_signal: AgentSignalState,
    pub(crate) sessions: SessionRegistry,
    pub(crate) clients: ClientRegistry,
    pub(crate) resize: ResizeState,
    pub(crate) io: IoHandles,
    pub(crate) session_creation_port: Option<Arc<dyn SessionCreationPort>>,
    pub(crate) target_registry_port: Option<Arc<dyn TargetRegistryPort>>,
    pub(crate) ingress_internal_tx: Mutex<Option<mpsc::Sender<InternalEvent>>>,
    pub(crate) process_monitor: Mutex<Option<crate::process_monitor::ProcessMonitor>>,
    /// Connection metadata for remote peers, keyed by `authority_node_id`.
    /// Accessed only from `StateEventLoop`.
    pub(crate) remote_node_connections: Mutex<HashMap<String, RemoteNodeConnectionInfo>>,
}

/// Runtime configuration for agent lifecycle signals.
pub(crate) struct AgentSignalState {
    /// Unix datagram socket path where agent hooks send lifecycle events.
    pub(crate) socket_path: String,
    /// Random token agents must include in their signal envelopes.
    pub(crate) token: String,
}

/// Registry of all sessions known to the ratatui node server.
///
/// Lock order within this registry: `sessions` -> `active_target` ->
/// `local_sessions` / `remote_sessions` / `authority_host_sessions`.
pub(crate) struct SessionRegistry {
    pub(crate) sessions: Mutex<HashMap<String, ManagedSessionRecord>>,
    pub(crate) active_target: Mutex<Option<String>>,
    pub(crate) local_sessions: Mutex<HashMap<String, Arc<RatatuiLocalSession>>>,
    pub(crate) authority_host_sessions: Mutex<HashMap<String, Arc<RatatuiAuthorityHostSession>>>,
    pub(crate) remote_sessions: Mutex<HashMap<String, Arc<RatatuiRemoteSession>>>,
}

/// Connected TUI clients and related counters.
pub(crate) struct ClientRegistry {
    pub(crate) client_count: AtomicUsize,
    pub(crate) clients: Arc<Mutex<Vec<ClientHandle>>>,
}

/// Last reported TUI client dimensions.
pub(crate) struct ResizeState {
    /// Last main-pane size reported by a TUI client. Used when the active target
    /// changes so the newly activated session can be resized immediately.
    pub(crate) last_client_resize: Mutex<Option<(u16, u16)>>,
}

/// Channel handles used by runtime loops. These are set once at startup and
/// read lazily by other code paths.
pub(crate) struct IoHandles {
    state_tx: Mutex<Option<mpsc::Sender<StateEvent>>>,
    authority_host_io_tx: Mutex<Option<super::authority_host_io_loop::AuthorityHostIoHandle>>,
    local_catalog_tx: Mutex<Vec<mpsc::Sender<LocalCatalogChangeRequest>>>,
}

impl SharedState {
    pub(crate) fn new(network: RemoteNetworkConfig) -> Result<Arc<Self>, LifecycleError> {
        let agent_signal_socket_path = super::socket::ratatui_socket_dir()
            .join(format!("signal-{port}.sock", port = network.port))
            .to_string_lossy()
            .to_string();
        let agent_signal_token = random_token()?;
        let public_endpoint_override = network.public_endpoint.clone();
        Ok(Arc::new(Self {
            network,
            public_endpoint_override: Mutex::new(public_endpoint_override),
            start_time: Instant::now(),
            shutdown: AtomicBool::new(false),
            agent_signal: AgentSignalState {
                socket_path: agent_signal_socket_path,
                token: agent_signal_token,
            },
            sessions: SessionRegistry {
                sessions: Mutex::new(HashMap::new()),
                active_target: Mutex::new(None),
                local_sessions: Mutex::new(HashMap::new()),
                authority_host_sessions: Mutex::new(HashMap::new()),
                remote_sessions: Mutex::new(HashMap::new()),
            },
            clients: ClientRegistry {
                client_count: AtomicUsize::new(0),
                clients: Arc::new(Mutex::new(Vec::new())),
            },
            resize: ResizeState {
                last_client_resize: Mutex::new(None),
            },
            io: IoHandles {
                state_tx: Mutex::new(None),
                authority_host_io_tx: Mutex::new(None),
                local_catalog_tx: Mutex::new(Vec::new()),
            },
            session_creation_port: None,
            target_registry_port: None,
            ingress_internal_tx: Mutex::new(None),
            process_monitor: Mutex::new(None),
            remote_node_connections: Mutex::new(HashMap::new()),
        }))
    }
}

impl IoHandles {
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

    pub(crate) fn add_local_catalog_tx(&self, tx: mpsc::Sender<LocalCatalogChangeRequest>) {
        if let Ok(mut guard) = self.local_catalog_tx.lock() {
            guard.push(tx);
        }
    }

    pub(crate) fn notify_local_catalog_changed(&self, reason: LocalCatalogChangeReason) {
        if let Ok(mut guard) = self.local_catalog_tx.lock() {
            guard.retain(|tx| {
                tx.send(LocalCatalogChangeRequest {
                    reason: reason.clone(),
                    ack_tx: None,
                })
                .is_ok()
            });
        }
    }
}

impl ClientRegistry {
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
}

impl SharedState {
    pub(crate) fn set_state_tx(&self, tx: mpsc::Sender<StateEvent>) {
        self.io.set_state_tx(tx);
    }

    pub(crate) fn state_sender(&self) -> mpsc::Sender<StateEvent> {
        self.io.state_sender()
    }

    pub(crate) fn set_authority_host_io_tx(
        &self,
        tx: super::authority_host_io_loop::AuthorityHostIoHandle,
    ) {
        self.io.set_authority_host_io_tx(tx);
    }

    pub(crate) fn authority_host_io_sender(
        &self,
    ) -> super::authority_host_io_loop::AuthorityHostIoHandle {
        self.io.authority_host_io_sender()
    }

    pub(crate) fn add_local_catalog_tx(&self, tx: mpsc::Sender<LocalCatalogChangeRequest>) {
        self.io.add_local_catalog_tx(tx);
    }

    pub(crate) fn notify_local_catalog_changed(&self, reason: LocalCatalogChangeReason) {
        self.io.notify_local_catalog_changed(reason);
    }

    pub(crate) fn set_process_monitor(&self, monitor: crate::process_monitor::ProcessMonitor) {
        if let Ok(mut guard) = self.process_monitor.lock() {
            *guard = Some(monitor);
        }
    }

    pub(crate) fn process_monitor(&self) -> Option<crate::process_monitor::ProcessMonitor> {
        self.process_monitor
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub(crate) fn record_remote_node_connection(
        &self,
        node_id: &str,
        info: RemoteNodeConnectionInfo,
    ) {
        if let Ok(mut guard) = self.remote_node_connections.lock() {
            guard.insert(node_id.to_string(), info);
        }
    }

    pub(crate) fn remote_node_connection(&self, node_id: &str) -> Option<RemoteNodeConnectionInfo> {
        self.remote_node_connections
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(node_id)
            .cloned()
    }

    pub(crate) fn remove_remote_node_connection(&self, node_id: &str) {
        if let Ok(mut guard) = self.remote_node_connections.lock() {
            guard.remove(node_id);
        }
    }

    pub(super) fn detach_all_clients(&self) {
        self.clients.detach_all_clients();
    }

    /// Mark a session as exited, remove its runtime, switch to the next
    /// available session, and shut down the server when the last session exits.
    pub(super) fn handle_session_exit(&self, target_id: &str) {
        if let Some(monitor) = self.process_monitor() {
            monitor.unregister_session(target_id);
        }

        let record = {
            let mut guard = self
                .sessions
                .sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            guard.remove(target_id)
        };
        let was_local = record
            .as_ref()
            .map(|r| r.address.transport() == &SessionTransport::Local)
            .unwrap_or(false);

        if was_local {
            let mut local_guard = self
                .sessions
                .local_sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            local_guard.remove(target_id);
            drop(local_guard);

            // Authority-host sessions are keyed by their short session id.
            if let Some(session_id) = target_id.rsplit_once(':').map(|(_, id)| id) {
                let mut host_guard = self
                    .sessions
                    .authority_host_sessions
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                host_guard.remove(session_id);
            }
        } else {
            let session = {
                let remote_guard = self
                    .sessions
                    .remote_sessions
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                remote_guard.get(target_id).cloned()
            };
            if let Some(session) = session {
                session.stop();
            }
            let mut remote_guard = self
                .sessions
                .remote_sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            remote_guard.remove(target_id);
            drop(remote_guard);
        }

        // Server lifetime is tied to local sessions. Remote sessions are views
        // into other hosts and should not keep this server alive when the last
        // local session exits. Both the local server and peer node servers
        // (started with --connect) exit once their last local session ends;
        // clients are expected to reconnect if they need a new session.
        let remaining_local: Vec<String> = {
            let guard = self
                .sessions
                .sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner());
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
            let mut active_guard = self
                .sessions
                .active_target
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if remaining_local.is_empty() {
                ERROR_LOG.log(format!(
                    "[ratatui-node] last local session {target_id} exited; shutting down"
                ));
                self.shutdown.store(true, Ordering::SeqCst);
                let _ = UnixStream::connect(super::socket::ratatui_socket_path(self.network.port));
                *active_guard = None;
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

    /// Record a terminal title change.
    ///
    /// The title is not used as the sidebar command name; the sidebar prefers
    /// agent/process detection so that user-typed text (e.g. in an agent prompt)
    /// does not overwrite the session label.
    pub(super) fn set_local_session_title(&self, target_id: &str, title: String) {
        ERROR_LOG.log(format!(
            "[ratatui-node] terminal title changed target_id={target_id} title={title}"
        ));
    }

    /// Update the inferred task state of a session and notify catalog sync.
    pub(super) fn set_session_task_state(
        &self,
        target_id: &str,
        task_state: ManagedSessionTaskState,
    ) {
        {
            let mut guard = self
                .sessions
                .sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(record) = guard.get_mut(target_id) {
                record.task_state = task_state;
            }
        }
        self.notify_local_catalog_changed(LocalCatalogChangeReason::LocalRuntimeChanged);
    }

    /// Update the displayed command name from agent/process detection and notify
    /// catalog sync.
    pub(super) fn set_session_command_name(&self, target_id: &str, command_name: String) {
        {
            let mut guard = self
                .sessions
                .sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(record) = guard.get_mut(target_id) {
                record.display_command_name = Some(command_name);
                if record.agent_command_name.is_none() {
                    if let Some(ref name) = record.display_command_name {
                        if crate::domain::agent_detector::accepts_at_reference(name) {
                            record.agent_command_name = Some(name.clone());
                        }
                    }
                }
            }
        }
        self.notify_local_catalog_changed(LocalCatalogChangeReason::LocalRuntimeChanged);
    }

    /// Clear the displayed command name and notify catalog sync.
    ///
    /// The persistent `agent_command_name` is intentionally kept so that paste
    /// formatting continues to work when the user is back at the shell prompt
    /// inside an agent session.
    pub(super) fn clear_session_command_name(&self, target_id: &str) {
        {
            let mut guard = self
                .sessions
                .sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(record) = guard.get_mut(target_id) {
                record.display_command_name = None;
            }
        }
        self.notify_local_catalog_changed(LocalCatalogChangeReason::LocalRuntimeChanged);
    }

    /// Update the current working directory of a session from shell integration
    /// and notify catalog sync so remote viewers see cwd changes.
    pub(super) fn set_session_current_path(&self, target_id: &str, current_path: PathBuf) {
        {
            let mut guard = self
                .sessions
                .sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(record) = guard.get_mut(target_id) {
                record.current_path = Some(current_path);
            }
        }
        self.notify_local_catalog_changed(LocalCatalogChangeReason::LocalRuntimeChanged);
    }

    /// Update a remote-peer catalog record from a published owner snapshot.
    /// Local records are never overwritten by remote publications.
    pub(super) fn update_remote_session_record(&self, record: ManagedSessionRecord) {
        if record.address.transport() != &SessionTransport::RemotePeer {
            return;
        }
        let target_id = record.address.qualified_target();
        let mut guard = self
            .sessions
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = guard.get_mut(&target_id) {
            existing.current_path = record.current_path;
            existing.task_state = record.task_state;
            existing.availability = record.availability;
            existing.command_name = record.command_name;
            existing.display_command_name = record.display_command_name;
            existing.agent_command_name = record.agent_command_name;
            existing.attached_clients = record.attached_clients;
            existing.window_count = record.window_count;
        } else {
            guard.insert(target_id, record);
        }
    }

    /// Return the authority id used for local sessions hosted by this server.
    pub(crate) fn local_authority_id(&self) -> String {
        format!("local#{}", self.network.port)
    }

    /// Return the public endpoint label advertised to remote peers.
    ///
    /// Uses the runtime override if one has been set, otherwise falls back to
    /// the CLI-provided or network-discovered value.
    pub(crate) fn advertised_public_endpoint_label(&self) -> String {
        let override_value = self
            .public_endpoint_override
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        override_value.unwrap_or_else(|| self.network.advertised_public_endpoint_label())
    }

    /// Update the runtime public endpoint override.
    ///
    /// This is the only way the public endpoint may change while the server is
    /// running; it must be called from `StateEventLoop` so that the single-writer
    /// invariant is preserved.
    pub(crate) fn set_public_endpoint_override(&self, endpoint: Option<String>) {
        if let Ok(mut guard) = self.public_endpoint_override.lock() {
            *guard = endpoint;
        }
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
                .sessions
                .local_sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            local_guard.insert(target_id.clone(), session);
        }

        {
            let mut guard = self
                .sessions
                .sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let record = ManagedSessionRecord {
                address: ManagedSessionAddress::local(self.local_authority_id(), session_id),
                selector: None,
                availability: SessionAvailability::Online,
                workspace_dir: None,
                workspace_key: None,
                session_role: Some(crate::domain::workspace::WorkspaceSessionRole::TargetHost),
                opened_by: Vec::new(),
                attached_clients: 0,
                window_count: 1,
                command_name: Some(command_name),
                display_command_name: None,
                agent_command_name: None,
                current_path: None,
                task_state: ManagedSessionTaskState::Input,
            };
            guard.insert(target_id.clone(), record);
        }

        *self
            .sessions
            .active_target
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(target_id.clone());
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
            let guard = self
                .sessions
                .sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            format!("{}", guard.len() + 1)
        };
        let target_id = ManagedSessionAddress::local(&authority_id, &id).qualified_target();

        let command_name = std::env::var("SHELL")
            .ok()
            .and_then(|s| s.rsplit_once('/').map(|(_, name)| name.to_string()))
            .unwrap_or_else(|| "bash".to_string());

        let signal_env = AgentSignalEnv {
            socket_path: self.agent_signal.socket_path.clone(),
            socket_name: format!("ratatui-{}", self.network.port),
            target_session_name: target_id.clone(),
            session_id: target_id.clone(),
            token: self.agent_signal.token.clone(),
        };
        let session = RatatuiAuthorityHostSession::spawn(
            id.clone(),
            command_name.clone(),
            cols,
            rows,
            signal_env,
        )?;

        {
            let mut guard = self
                .sessions
                .sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let record = ManagedSessionRecord {
                address: ManagedSessionAddress::local(authority_id, &id),
                selector: None,
                availability: SessionAvailability::Online,
                workspace_dir: None,
                workspace_key: None,
                session_role: Some(crate::domain::workspace::WorkspaceSessionRole::TargetHost),
                opened_by: Vec::new(),
                attached_clients: 0,
                window_count: 1,
                command_name: Some(command_name),
                display_command_name: None,
                agent_command_name: None,
                current_path: None,
                // Authority-host sessions are shell-only until a command is
                // launched, just like local sessions. Start at Input so remote
                // viewers do not see a transient Running state before the first
                // process-monitor refresh arrives.
                task_state: ManagedSessionTaskState::Input,
            };
            guard.insert(target_id.clone(), record);
        }

        *self
            .sessions
            .active_target
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(target_id.clone());

        // The caller (StateEventLoop) is responsible for registering the PTY
        // master fd with `AuthorityHostIoLoop` and then inserting the session
        // into `authority_host_sessions`.
        Ok((id, session, target_id))
    }

    /// Resize the active local session, if any.
    pub(crate) fn resize_active_local_session(&self, cols: u16, rows: u16) {
        let active = self
            .sessions
            .active_target
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(target) = active {
            let guard = self
                .sessions
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
    ///
    /// This does **not** send the OpenMirrorRequest.  The mirror is opened with
    /// the real TUI dimensions the first time the session becomes active or a
    /// resize arrives, avoiding the stale 80x24 startup frame that used to leave
    /// the top half of the main pane blank.
    pub(crate) fn ensure_remote_session(
        self: &Arc<Self>,
        record: &ManagedSessionRecord,
    ) -> Result<String, LifecycleError> {
        let target_id = record.address.qualified_target();
        {
            let guard = self
                .sessions
                .remote_sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if guard.contains_key(&target_id) {
                return Ok(target_id);
            }
        }
        let session =
            RatatuiRemoteSession::open(record, &self.workspace_id(), &self.network, self, None)?;
        {
            let mut guard = self
                .sessions
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
                .sessions
                .remote_sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            guard.get(target_id).cloned()
        };
        if let Some(session) = session {
            session.feed_input(bytes);
        }
    }

    /// Forward a pasted file to a specific remote session keyed by qualified target.
    pub(crate) fn feed_remote_session_paste_file(
        &self,
        target_id: &str,
        filename_hint: &str,
        bytes: &[u8],
    ) {
        let session = {
            let guard = self
                .sessions
                .remote_sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            guard.get(target_id).cloned()
        };
        if let Some(session) = session {
            session.send_paste_file(filename_hint, bytes);
        } else {
            ERROR_LOG.log(format!(
                "[ratatui-node] feed_remote_session_paste_file: no remote session for {target_id}"
            ));
        }
    }

    /// Remove all remote-peer sessions whose authority id matches the given
    /// node.  Called when a remote peer reconnects with a new node instance,
    /// which means any previous sessions hosted by that peer are stale.
    pub(crate) fn remove_remote_sessions_for_node(&self, node_id: &str) {
        let to_remove: Vec<String> = {
            let guard = self
                .sessions
                .sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner());
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

    /// Resize the active remote session, or open it with the given dimensions
    /// if this is the first time it is being viewed.
    pub(crate) fn resize_active_remote_session(&self, cols: u16, rows: u16) {
        let active = self
            .sessions
            .active_target
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let Some(target) = active else {
            return;
        };
        let session = {
            let guard = self
                .sessions
                .remote_sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            guard.get(&target).cloned()
        };
        if let Some(session) = session {
            // Drop the remote_sessions lock before doing I/O or calling into
            // the observer.  The opened flag guarantees the mirror is opened
            // exactly once even if resize races with activation.
            if !session.is_opened() {
                session.open_mirror(cols, rows);
            } else {
                session.resize(cols, rows);
                session.resize_local_screen(cols, rows);
            }
        }
    }

    /// Return the full scrollback history plus visible screen for a target.
    pub(crate) fn history_for_target(&self, target_id: &str) -> Option<(Vec<String>, Vec<String>)> {
        let guard = self
            .sessions
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let transport = guard.get(target_id).map(|s| s.address.transport().clone());
        drop(guard);
        match transport {
            Some(SessionTransport::RemotePeer) => {
                let guard = self
                    .sessions
                    .remote_sessions
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                guard.get(target_id).map(|s| s.history_snapshot())
            }
            Some(SessionTransport::Local) => {
                let guard = self
                    .sessions
                    .local_sessions
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                guard.get(target_id).map(|s| s.history_snapshot())
            }
            None => None,
        }
    }
}

fn random_token() -> Result<String, LifecycleError> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|error| {
        LifecycleError::Io(
            "failed to generate agent signal token".to_string(),
            std::io::Error::other(error),
        )
    })?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

#[derive(Debug, Clone)]
pub(crate) struct RatatuiTargetCatalogGateway {
    remote_owner: RemoteRuntimeOwnerRuntime,
}

impl RatatuiTargetCatalogGateway {
    pub(crate) fn new(remote_owner: RemoteRuntimeOwnerRuntime) -> Self {
        Self { remote_owner }
    }
}

impl crate::ports::target_registry::TargetCatalogGateway for RatatuiTargetCatalogGateway {
    type Error = LifecycleError;

    fn list_targets(&self) -> Result<Vec<ManagedSessionRecord>, LifecycleError> {
        Ok(self.remote_owner.try_snapshot()?.sessions)
    }
}

impl RatatuiNodeRuntime {
    pub fn from_network(
        network: RemoteNetworkConfig,
        remote_owner: RemoteRuntimeOwnerRuntime,
        session_creation_port: Arc<dyn SessionCreationPort>,
        target_registry_port: Arc<dyn TargetRegistryPort>,
        hooks_config_ports: Vec<Box<dyn HooksConfigPort>>,
        settings_store: crate::infra::settings_store::SettingsStore,
    ) -> Result<Self, LifecycleError> {
        let mut shared = SharedState::new(network.clone())?;
        if let Some(state) = Arc::get_mut(&mut shared) {
            state.session_creation_port = Some(session_creation_port);
            state.target_registry_port = Some(target_registry_port);
        }

        // Start the event-driven process monitor before any local session is
        // created so every session can be registered as soon as it spawns.
        let process_monitor = crate::process_monitor::ProcessMonitor::start(shared.clone())?;
        shared.set_process_monitor(process_monitor);

        // Create the default local session with a reasonable initial size.
        // The client will send a RESIZE once it knows the terminal dimensions.
        // Only the local TUI server creates a local session here; peer node
        // servers (with --connect) create an authority-host session once the
        // event loops are running, because authority-host sessions must be
        // registered with AuthorityHostIoLoop.
        if network.node_id.is_none() && network.connect.is_none() {
            let _ = shared.create_local_session(DEFAULT_SESSION_ID, 80, 24);
        }

        Ok(Self {
            network: network.clone(),
            shared,
            remote_owner,
            hooks_config_ports,
            settings_store,
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
            crate::infra::best_effort::create_dir_all(parent);
        }

        // Remove stale socket before binding.
        crate::infra::best_effort::remove_file(&socket_path);

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
        let (ingress_catalog_tx, ingress_catalog_rx) =
            std::sync::mpsc::channel::<LocalCatalogChangeRequest>();
        let authority_host_io = AuthorityHostIoLoop::start(self.shared.clone())?;
        let client_writer = ClientWriter::start();
        let state_event_loop = StateEventLoop::start(
            self.shared.clone(),
            catalog_tx.clone(),
            &authority_host_io,
            client_writer.clone(),
            self.remote_owner.clone(),
            self.settings_store.clone(),
        )?;
        self.shared.set_state_tx(state_event_loop.sender());
        self.shared
            .set_authority_host_io_tx(authority_host_io.sender());
        self.shared.add_local_catalog_tx(catalog_tx);
        self.shared.add_local_catalog_tx(ingress_catalog_tx);

        // Monitor upstream connectivity so the state loop can distinguish a
        // transient control-plane outage from a permanent remote host failure.
        let _network_probe = NetworkProbe::start(state_event_loop.sender());

        // Migrate any legacy file-based secrets into the OS keyring before
        // attempting reconnections that may need them.
        if let Err(error) =
            crate::host::ssh::remote_host_secret_store::migrate_file_secrets_to_keyring()
        {
            ERROR_LOG.log(format!("[ratatui-node] secret migration failed: {error}"));
        }

        // Ensure the operator key used for gRPC challenge-response auth exists in
        // the keyring before any remote peer connection is attempted.
        if let Err(error) = crate::infra::operator_auth::ensure_operator_key_in_keyring() {
            ERROR_LOG.log(format!("[ratatui-node] operator key setup failed: {error}"));
        }

        // Reconnect to outbound-dial hosts that were active when the control
        // plane last shut down, or retry them once the network recovers.
        let _ = state_event_loop
            .sender()
            .send(StateEvent::ReconnectSnapshotHosts);

        // Peer node servers host a default authority-host session for remote
        // viewers. Create it now that the IO loops are running; it will be
        // published through the local catalog to the remote authority.
        //
        // This applies to both `--connect` peers (which actively dial the
        // control plane) and outbound-dial peers (which wait for the control
        // plane to dial them). In both cases the remote peer must expose at
        // least one session for the viewer to attach to.
        if self.network.node_id.is_some() {
            let (reply_tx, _reply_rx) = std::sync::mpsc::channel();
            let _ = self
                .shared
                .state_sender()
                .send(StateEvent::CreateAuthorityHostSession {
                    request_id: "default-peer-session".to_string(),
                    cols: 80,
                    rows: 24,
                    reply_tx,
                });
        }

        // Start the agent signal listener so agent hooks can deliver lifecycle
        // events (UserPromptSubmit, PermissionRequest, etc.) to this server.
        let signal_server = match AgentSignalServer::start(self.shared.clone()) {
            Ok(server) => {
                ERROR_LOG.log(format!(
                    "[ratatui-node] agent signal socket={}",
                    self.shared.agent_signal.socket_path
                ));
                Some(server)
            }
            Err(error) => {
                ERROR_LOG.log(format!(
                    "[ratatui-node] failed to start agent signal server: {error}"
                ));
                None
            }
        };

        // Install agent hooks into claude/codex/kimi config files.  The hooks
        // reuse the existing `waitagent-agent-signal-send` binary and the
        // environment variables exported into each local PTY session.
        for hook in &self.hooks_config_ports {
            if let Err(error) = hook.reconcile() {
                ERROR_LOG.log(format!(
                    "[ratatui-node] failed to install {} hooks: {error}",
                    hook.agent_name()
                ));
            }
        }

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
            RatatuiLocalSessionCatalog::new(shared.clone()),
        );
        let _ingress_guard = match ingress_runtime.start(ingress_catalog_rx) {
            Ok(guard) => {
                // Bind the same local control socket that legacy sidecar
                // ingress owners use, so __remote-session-creation and Ctrl+W
                // connect can reach this single-process server.
                let owner_socket_path = remote_node_ingress_owner_socket_path(&self.network);
                crate::infra::best_effort::remove_file(&owner_socket_path);
                if let Ok(owner_listener) =
                    std::os::unix::net::UnixListener::bind(&owner_socket_path)
                {
                    if let Some(owner_tx) = guard.owner_event_sender() {
                        if let Ok(mut guard) = self.shared.ingress_internal_tx.lock() {
                            *guard = Some(owner_tx.clone());
                        }
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

        let clients = self.shared.clients.clients.clone();
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
                    ERROR_LOG.log_error(format!("[ratatui-node] accept error: {error:?}"));
                }
            }
        }

        if let Some(signal_server) = signal_server {
            signal_server.cleanup();
        }
        crate::infra::best_effort::remove_file(&socket_path);
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

#[cfg(test)]
mod runtime_tests {
    use super::*;
    use crate::cli::RemoteNetworkConfig;
    use crate::domain::session_catalog::{
        ManagedSessionAddress, ManagedSessionRecord, ManagedSessionTaskState, SessionAvailability,
    };
    use crate::ratatui_node::snapshot::build_snapshot;
    use std::sync::atomic::Ordering;

    fn sample_record() -> ManagedSessionRecord {
        ManagedSessionRecord {
            address: ManagedSessionAddress::local("local#0", "1"),
            selector: None,
            availability: SessionAvailability::Online,
            workspace_dir: None,
            workspace_key: None,
            session_role: Some(crate::domain::workspace::WorkspaceSessionRole::TargetHost),
            opened_by: Vec::new(),
            attached_clients: 0,
            window_count: 1,
            command_name: Some("bash".to_string()),
            display_command_name: None,
            agent_command_name: None,
            current_path: None,
            task_state: ManagedSessionTaskState::Input,
        }
    }

    #[test]
    fn shared_state_snapshot_includes_all_sessions() {
        let network = RemoteNetworkConfig::default();
        let shared = SharedState::new(network).expect("SharedState::new should succeed");
        let record = sample_record();
        let target_id = record.address.qualified_target();

        {
            let mut guard = shared
                .sessions
                .sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            guard.insert(target_id.clone(), record);
        }

        let snapshot = build_snapshot(0, &shared);
        assert!(
            snapshot.sessions.iter().any(|s| s.id == target_id),
            "snapshot should include the inserted session"
        );
    }

    #[test]
    fn shared_state_client_count_tracks_attachments() {
        let network = RemoteNetworkConfig::default();
        let shared = SharedState::new(network).expect("SharedState::new should succeed");

        shared.clients.client_count.fetch_add(1, Ordering::SeqCst);
        assert_eq!(shared.clients.client_count.load(Ordering::SeqCst), 1);

        shared.clients.client_count.fetch_sub(1, Ordering::SeqCst);
        assert_eq!(shared.clients.client_count.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn shared_state_active_target_can_be_set() {
        let network = RemoteNetworkConfig::default();
        let shared = SharedState::new(network).expect("SharedState::new should succeed");
        let target = "local#0:1".to_string();

        {
            let mut guard = shared
                .sessions
                .active_target
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            *guard = Some(target.clone());
        }

        let snapshot = build_snapshot(0, &shared);
        assert_eq!(snapshot.active_target, Some(target));
    }

    #[test]
    fn shared_state_public_endpoint_override_is_advertised() {
        let mut network = RemoteNetworkConfig::default();
        network.public_endpoint = Some("cli.example:17474".to_string());
        let shared = SharedState::new(network).expect("SharedState::new should succeed");

        assert_eq!(
            shared.advertised_public_endpoint_label(),
            "cli.example:17474"
        );

        shared.set_public_endpoint_override(Some("runtime.example:7474".to_string()));
        assert_eq!(
            shared.advertised_public_endpoint_label(),
            "runtime.example:7474"
        );

        shared.set_public_endpoint_override(None);
        assert_eq!(
            shared.advertised_public_endpoint_label(),
            "cli.example:17474"
        );
    }
}
