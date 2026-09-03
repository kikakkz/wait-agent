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

    #[cfg(windows)]
    pub fn tcp_addr(&self) -> std::net::SocketAddr {
        std::net::SocketAddr::from(([127, 0, 0, 1], self.port))
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
/// first line of response.
pub fn send_node_command(port: u16, command: &str) -> Result<String, LifecycleError> {
    send_node_command_impl(LocalIpcAddr::node(port), command)
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
    let mut line = String::new();
    reader.read_line(&mut line).map_err(|error| {
        LifecycleError::Io("failed to read ratatui node response".to_string(), error)
    })?;
    Ok(line.trim().to_string())
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
    let mut line = String::new();
    reader.read_line(&mut line).map_err(|error| {
        LifecycleError::Io("failed to read ratatui node response".to_string(), error)
    })?;
    Ok(line.trim().to_string())
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
