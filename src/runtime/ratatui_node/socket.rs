use crate::lifecycle::LifecycleError;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

/// Returns the per-user directory that holds ratatui sockets.
///
/// Mirrors tmux's convention: a user-specific subdirectory under the system
/// temp directory so sockets from different users do not collide.
pub fn ratatui_socket_dir() -> PathBuf {
    std::env::temp_dir().join(format!("waitagent-ratatui-{}", effective_uid()))
}

/// Returns a stable Unix socket path for a given listener port.
pub fn ratatui_socket_path(port: u16) -> PathBuf {
    ratatui_socket_dir().join(format!("{port}.sock"))
}

fn effective_uid() -> u32 {
    // SAFETY: geteuid has no arguments and is always safe to call; it returns
    // the real user ID of the calling process.
    unsafe { libc::geteuid() }
}

/// Returns true if a ratatui node server appears to be listening on the port.
pub fn node_is_running(port: u16) -> bool {
    let socket_path = ratatui_socket_path(port);
    if !socket_path.exists() {
        return false;
    }
    UnixStream::connect(&socket_path).is_ok()
}

/// Removes the socket file for a port, if any.
pub fn remove_node_socket(port: u16) {
    let _ = std::fs::remove_file(ratatui_socket_path(port));
}

/// Sends a one-shot control command to the server on `port` and returns the
/// first line of response. Used by `ls`, `detach`, `stop`, `list-sessions`.
pub fn send_node_command(port: u16, command: &str) -> Result<String, LifecycleError> {
    let socket_path = ratatui_socket_path(port);
    let mut stream = UnixStream::connect(&socket_path).map_err(|error| {
        LifecycleError::Io(
            format!(
                "failed to connect to ratatui node socket {}",
                socket_path.display()
            ),
            error,
        )
    })?;
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .map_err(|error| {
            LifecycleError::Io(
                "failed to set read timeout on ratatui node socket".to_string(),
                error,
            )
        })?;
    writeln!(stream, "{command}").map_err(|error| {
        LifecycleError::Io("failed to send command to ratatui node".to_string(), error)
    })?;
    stream.flush().map_err(|error| {
        LifecycleError::Io("failed to flush command to ratatui node".to_string(), error)
    })?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).map_err(|error| {
        LifecycleError::Io("failed to read ratatui node response".to_string(), error)
    })?;
    Ok(line.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::{ratatui_socket_dir, ratatui_socket_path};

    #[test]
    fn socket_path_is_in_user_specific_dir() {
        let path = ratatui_socket_path(7575);
        assert!(path.starts_with(ratatui_socket_dir()));
        assert!(path.as_os_str().to_string_lossy().ends_with(".sock"));
    }

    #[test]
    fn socket_path_differs_by_port() {
        let path1 = ratatui_socket_path(7575);
        let path2 = ratatui_socket_path(7576);
        assert_ne!(path1, path2);
    }
}
