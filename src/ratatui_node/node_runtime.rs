//! Compatibility shim: re-export the ratatui node public API so existing call
//! sites that import through `ratatui_node_runtime` keep working.
pub use super::{
    node_is_running, ratatui_socket_dir, remove_node_socket, send_node_command, ControlResponse,
    HistoryResponse, RatatuiNodeRuntime, RatatuiSnapshot, ServerMessageJson, ServerStatus,
    SessionView,
};
