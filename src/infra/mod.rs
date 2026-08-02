pub mod error_log;
pub mod per_server_geometry_store;
pub mod remote_grpc_proto;
pub mod remote_grpc_transport;

// Preserved tmux infra stubs: the ratatui local path no longer uses these,
// but several remote-ratatui modules (e.g. target_registry_service,
// remote_server_console_runtime, remote_main_slot_ingress_runtime) still
// depend on TmuxSessionGateway / TmuxError types. They will be removed once
// the remote path is fully decoupled from tmux abstractions.
mod tmux_backend;
mod tmux_error;
mod tmux_types;

pub mod remote_protocol;
pub mod remote_transport_codec;
pub mod tmux;
