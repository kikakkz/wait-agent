//! Cross-platform local IPC for the ratatui node server and its clients.
//!
//! Unix uses Unix Domain Sockets. Windows (stage 2) uses TCP loopback plus a
//! marker file for discovery.

use crate::lifecycle::LifecycleError;
use std::io::{self, BufRead, BufReader, Write};
use std::path::PathBuf;
use std::time::Duration;

/// Identifies a local ratatui node server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalIpcAddr {
    port: u16,
}

impl LocalIpcAddr {
    pub fn node(port: u16) -> Self {
        Self { port }
    }

    #[allow(dead_code)]
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Unix: the UDS path. Windows: the marker file path.
    pub fn path(&self) -> PathBuf {
        marker_dir().join(format!("{}.{}", self.port, path_extension()))
    }

    #[cfg(unix)]
    #[allow(dead_code)]
    pub fn socket_path(&self) -> PathBuf {
        self.path()
    }

    /// Windows local IPC runs on a loopback TCP port derived from the node
    /// port. The offset keeps it clear of the node's remote gRPC listener
    /// (which binds the node port on all interfaces) and of third-party
    /// services that occupy the node port (e.g. 7474).
    #[cfg(windows)]
    pub fn tcp_addr(&self) -> std::net::SocketAddr {
        std::net::SocketAddr::from((
            [127, 0, 0, 1],
            self.port.saturating_add(LOCAL_IPC_PORT_OFFSET),
        ))
    }
}

#[cfg(unix)]
fn path_extension() -> &'static str {
    "sock"
}

#[cfg(windows)]
fn path_extension() -> &'static str {
    "port"
}

/// Offset from the node port to the loopback TCP port used for Windows local
/// IPC. The remote gRPC listener binds the node port on all interfaces, so
/// local IPC must live elsewhere; 20000 mirrors the offset scheme used for
/// remote-control listeners (`RemoteControlKind::windows_port_offset`).
#[cfg(windows)]
const LOCAL_IPC_PORT_OFFSET: u16 = 20_000;

/// Directory that holds per-node marker files (and UDS files on Unix).
pub fn marker_dir() -> PathBuf {
    std::env::temp_dir().join(format!("waitagent-ratatui-{}", user_tag()))
}

fn user_tag() -> String {
    #[cfg(unix)]
    {
        // SAFETY: geteuid has no arguments and is always safe to call.
        unsafe { libc::geteuid().to_string() }
    }
    #[cfg(windows)]
    {
        std::env::var("USERNAME")
            .or_else(|_| std::env::var("USER"))
            .unwrap_or_else(|_| "unknown".to_string())
    }
}

/// Returns true if a ratatui node server appears to be listening on the port.
pub fn node_is_running(port: u16) -> bool {
    probe_node(LocalIpcAddr::node(port))
}

/// Best-effort connection to the local listener used to wake `accept()` during
/// shutdown. Errors are ignored because the server may already be exiting.
pub fn wake_listener(port: u16) {
    let _ = probe_node_connect(LocalIpcAddr::node(port));
}

#[cfg(unix)]
fn probe_node_connect(addr: LocalIpcAddr) -> io::Result<()> {
    use std::os::unix::net::UnixStream;
    UnixStream::connect(addr.path())?;
    Ok(())
}

#[cfg(windows)]
fn probe_node_connect(addr: LocalIpcAddr) -> io::Result<()> {
    use std::net::TcpStream;
    TcpStream::connect_timeout(&addr.tcp_addr(), Duration::from_millis(200))?;
    Ok(())
}

#[cfg(unix)]
fn probe_node(addr: LocalIpcAddr) -> bool {
    use std::os::unix::net::UnixStream;
    addr.path().exists() && UnixStream::connect(addr.path()).is_ok()
}

#[cfg(windows)]
fn probe_node(addr: LocalIpcAddr) -> bool {
    use std::net::TcpStream;
    let tcp = addr.tcp_addr();
    TcpStream::connect_timeout(&tcp, Duration::from_millis(200)).is_ok()
}

/// Removes the marker/socket file for a port, if any.
pub fn remove_node_marker(port: u16) {
    crate::infra::best_effort::remove_file(LocalIpcAddr::node(port).path());
}

/// Writes a marker file indicating that a server is running on `port`.
#[cfg(windows)]
pub fn write_node_marker(port: u16) -> io::Result<()> {
    let path = LocalIpcAddr::node(port).path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let pid = std::process::id();
    let started = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    std::fs::write(&path, format!("{pid}\n{started}\n"))
}

#[cfg(unix)]
pub fn write_node_marker(_port: u16) -> io::Result<()> {
    // On Unix the UDS file itself serves as the running marker.
    Ok(())
}

/// Sends a one-shot control command to the server on `port` and returns the
/// response line. Server-pushed messages (snapshots, history) that arrive
/// before the response are skipped.
pub fn send_node_command(port: u16, command: &str) -> Result<String, LifecycleError> {
    send_node_command_impl(LocalIpcAddr::node(port), command)
}

/// Returns true when a wire line is a server-pushed message (snapshot or
/// history) rather than a control response. One-shot command clients can
/// receive pushed broadcasts while waiting for the response.
fn is_push_message(line: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
        return false;
    };
    matches!(
        value.get("type").and_then(|tag| tag.as_str()),
        Some("snapshot") | Some("history")
    )
}

/// Read lines until the first non-push message, which is the control response.
/// Lines that fail JSON parsing are treated as legacy plain-text responses.
fn read_response_line(reader: &mut impl BufRead) -> io::Result<String> {
    let mut line = String::new();
    loop {
        line.clear();
        reader.read_line(&mut line)?;
        if !is_push_message(line.trim()) {
            return Ok(line.trim().to_string());
        }
    }
}

#[cfg(unix)]
fn send_node_command_impl(addr: LocalIpcAddr, command: &str) -> Result<String, LifecycleError> {
    use std::os::unix::net::UnixStream;

    let path = addr.path();
    let mut stream = UnixStream::connect(&path).map_err(|error| {
        LifecycleError::Io(
            format!(
                "failed to connect to ratatui node socket {}",
                path.display()
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
    read_response_line(&mut reader).map_err(|error| {
        LifecycleError::Io("failed to read ratatui node response".to_string(), error)
    })
}

#[cfg(windows)]
fn send_node_command_impl(addr: LocalIpcAddr, command: &str) -> Result<String, LifecycleError> {
    use std::net::TcpStream;

    let tcp = addr.tcp_addr();
    let mut stream = TcpStream::connect_timeout(&tcp, Duration::from_secs(2)).map_err(|error| {
        LifecycleError::Io(
            format!("failed to connect to ratatui node TCP {tcp}"),
            error,
        )
    })?;
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .map_err(|error| {
            LifecycleError::Io(
                "failed to set read timeout on ratatui node TCP".to_string(),
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
    read_response_line(&mut reader).map_err(|error| {
        LifecycleError::Io("failed to read ratatui node response".to_string(), error)
    })
}

/// Lists ports that have a marker file in the runtime directory.
pub fn running_node_ports() -> Vec<u16> {
    let mut ports = Vec::new();
    let Ok(entries) = std::fs::read_dir(marker_dir()) else {
        return ports;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(stem) = name.strip_suffix(&format!(".{}", path_extension())) else {
            continue;
        };
        if let Ok(port) = stem.parse::<u16>() {
            ports.push(port);
        }
    }
    ports.sort();
    ports
}

#[cfg(unix)]
pub(crate) mod unix;
#[cfg(windows)]
pub(crate) mod windows;

#[cfg(unix)]
pub use unix::{LocalListener, LocalStream};
#[cfg(windows)]
pub use windows::{LocalListener, LocalStream};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_push_message_detects_snapshot_and_history() {
        assert!(is_push_message(r#"{"type":"snapshot","payload":{}}"#));
        assert!(is_push_message(r#"{"type":"history","payload":{}}"#));
    }

    #[test]
    fn is_push_message_rejects_responses_and_plain_text() {
        assert!(!is_push_message(
            r#"{"type":"response","payload":{"ok":true}}"#
        ));
        assert!(!is_push_message("OK connected"));
        assert!(!is_push_message("ERR boom"));
        assert!(!is_push_message(""));
    }

    #[test]
    fn read_response_line_skips_pushed_messages() {
        let wire = concat!(
            r#"{"type":"snapshot","payload":{}}"#,
            "\n",
            r#"{"type":"history","payload":{}}"#,
            "\n",
            r#"{"type":"response","payload":{"ok":true}}"#,
            "\n"
        );
        let mut reader = BufReader::new(io::Cursor::new(wire.as_bytes()));
        let line = read_response_line(&mut reader).expect("response line");
        assert_eq!(line, r#"{"type":"response","payload":{"ok":true}}"#);
    }

    #[test]
    fn read_response_line_accepts_legacy_plain_text() {
        let mut reader = BufReader::new(io::Cursor::new(b"OK connected\n"));
        let line = read_response_line(&mut reader).expect("response line");
        assert_eq!(line, "OK connected");
    }
}
