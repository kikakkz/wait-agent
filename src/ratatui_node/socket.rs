//! Local ratatui node discovery and one-shot command helpers.
//!
//! Platform-specific IPC implementation lives in `crate::platform::local_ipc`;
//! this module preserves the existing public API so callers do not change.

use crate::platform::local_ipc::{marker_dir, remove_node_marker, LocalIpcAddr};
use std::path::PathBuf;

/// Returns the per-user directory that holds ratatui sockets / marker files.
pub fn ratatui_socket_dir() -> PathBuf {
    marker_dir()
}

/// Returns a stable path for a given listener port.
///
/// On Unix this is the UDS file. On Windows this is a marker file used for
/// discovery; the actual IPC uses TCP loopback.
#[allow(dead_code)]
pub fn ratatui_socket_path(port: u16) -> PathBuf {
    LocalIpcAddr::node(port).path()
}

/// Returns true if a ratatui node server appears to be listening on the port.
pub use crate::platform::local_ipc::node_is_running;

/// Removes the socket/marker file for a port, if any.
pub fn remove_node_socket(port: u16) {
    remove_node_marker(port);
}

/// Sends a one-shot control command to the server on `port` and returns the
/// first line of response.
pub use crate::platform::local_ipc::send_node_command;

#[cfg(test)]
mod tests {
    use super::{ratatui_socket_dir, ratatui_socket_path};

    #[test]
    fn socket_path_is_in_user_specific_dir() {
        let path = ratatui_socket_path(7575);
        assert!(path.starts_with(ratatui_socket_dir()));
    }

    #[test]
    fn socket_path_differs_by_port() {
        let path1 = ratatui_socket_path(7575);
        let path2 = ratatui_socket_path(7576);
        assert_ne!(path1, path2);
    }
}
