pub mod remote_authority;
pub mod remote_host;
pub use remote_authority::remote_authority_connection_runtime;
pub use remote_authority::remote_authority_transport_runtime;
// remote_authority_target_host_runtime is tmux-backed and not re-exported here.

pub mod remote_main_slot;
pub use remote_main_slot::remote_main_slot_runtime;

pub mod remote_node;
pub use remote_node::remote_node_ingress_server_runtime;
pub use remote_node::remote_node_session_runtime;
pub use remote_node::remote_node_session_sync_runtime;
pub use remote_node::remote_node_transport_runtime;
pub use remote_node::remote_runtime_owner_runtime;
pub use remote_node::remote_workspace_socket_registry_runtime;

pub mod remote_publication;
pub use remote_publication::remote_target_publication_runtime;
pub use remote_publication::remote_transport_runtime;

// Ratatui migration path
pub mod ratatui_client_runtime;
pub mod ratatui_node;
pub mod ratatui_node_runtime;
pub mod ratatui_workspace_runtime;

// Preserved workspace helpers still required by the ratatui remote path;
// the tmux-backed workspace runtimes were removed in Phase 4.
pub mod workspace;
pub use workspace::sidecar_process_runtime;

// Standalone modules remaining in runtime root
pub mod agent_signal_sender_bundle;
pub mod remote_observer_runtime;

pub(crate) mod current_executable;
pub(crate) mod network_state_runtime;
