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
use crate::runtime::remote_publication::ratatui_target_publication_backend::
    RatatuiRemoteTargetPublicationBackend;
use crate::runtime::remote_publication::remote_target_publication_runtime::
    RemoteTargetPublicationRuntime;
use crate::runtime::remote_runtime_owner_runtime::RemoteRuntimeOwnerRuntime;
use std::collections::HashMap;
use std::os::unix::net::UnixListener;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use super::client::ClientHandle;

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
}

impl SharedState {
    pub(crate) fn broadcast_snapshot(&self) -> Result<(), LifecycleError> {
        super::snapshot::broadcast_snapshot(&self.clients, self)
    }
}

impl RatatuiNodeRuntime {
    pub fn from_network(network: RemoteNetworkConfig) -> Result<Self, LifecycleError> {
        let mut sessions = HashMap::new();
        let default_record = default_local_session_record(DEFAULT_SESSION_ID);
        let active_target = Some(default_record.address.qualified_target());
        sessions.insert(DEFAULT_SESSION_ID.to_string(), default_record);

        let remote_owner = RemoteRuntimeOwnerRuntime::from_build_env_with_network(network.clone())?;

        Ok(Self {
            network: network.clone(),
            shared: Arc::new(SharedState {
                network,
                sessions: Mutex::new(sessions),
                active_target: Mutex::new(active_target),
                client_count: AtomicUsize::new(0),
                clients: Arc::new(Mutex::new(Vec::new())),
                start_time: Instant::now(),
                shutdown: AtomicBool::new(false),
            }),
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
            let backend = RatatuiRemoteTargetPublicationBackend::new(shared.clone(), sync_network.clone());
            let publication_runtime =
                RemoteTargetPublicationRuntime::with_network_and_backend(sync_network.clone(), backend)?;
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
        let ingress_backend = RatatuiRemoteTargetPublicationBackend::new(shared.clone(), ingress_network.clone());
        let ingress_publication_runtime =
            RemoteTargetPublicationRuntime::with_network_and_backend(ingress_network.clone(), ingress_backend)?;
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
                if let Ok(owner_listener) = std::os::unix::net::UnixListener::bind(&owner_socket_path)
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
                        let client_id = super::client::NEXT_CLIENT_ID.fetch_add(1, Ordering::SeqCst);
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

pub(crate) fn default_local_session_record(session_id: &str) -> ManagedSessionRecord {
    ManagedSessionRecord {
        address: ManagedSessionAddress::local_tmux(session_id, "main"),
        selector: None,
        availability: SessionAvailability::Online,
        workspace_dir: None,
        workspace_key: None,
        session_role: None,
        opened_by: Vec::new(),
        attached_clients: 0,
        window_count: 1,
        command_name: Some("bash".to_string()),
        display_command_name: None,
        current_path: None,
        task_state: ManagedSessionTaskState::Input,
    }
}
