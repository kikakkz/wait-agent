//! Cross-platform control sockets for remote node ingress / runtime owner.
//!
//! Unix uses UDS files under the temp dir. Windows uses TCP loopback on
//! derived ports to avoid depending on UDS.
//!
//! The Unix file names produced here must stay identical to the historical
//! names because other tools and tests locate them by convention:
//! - ingress owner:  `waitagent-remote-node-ingress-<sanitized>.sock`
//! - runtime owner:  `waitagent-remote-runtime-owner-<sanitized>.sock`

use crate::cli::RemoteNetworkConfig;
use std::io;
use std::path::{Path, PathBuf};

/// Address of a remote-control listener.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RemoteControlAddr {
    #[cfg(unix)]
    Unix(PathBuf),
    #[cfg(windows)]
    Tcp(std::net::SocketAddr),
}

impl std::fmt::Display for RemoteControlAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_arg_string())
    }
}

impl RemoteControlAddr {
    #[cfg(unix)]
    pub fn unix_path(&self) -> Option<&PathBuf> {
        match self {
            RemoteControlAddr::Unix(path) => Some(path),
        }
    }

    /// Serializes the address for passing to a child process as a CLI arg.
    pub fn to_arg_string(&self) -> String {
        match self {
            #[cfg(unix)]
            RemoteControlAddr::Unix(path) => path.display().to_string(),
            #[cfg(windows)]
            RemoteControlAddr::Tcp(addr) => addr.to_string(),
        }
    }

    /// Parses an address previously produced by [`RemoteControlAddr::to_arg_string`].
    pub fn from_arg_string(value: &str) -> io::Result<Self> {
        match () {
            #[cfg(unix)]
            () => Ok(RemoteControlAddr::Unix(PathBuf::from(value))),
            #[cfg(windows)]
            () => value
                .parse::<std::net::SocketAddr>()
                .map(RemoteControlAddr::Tcp)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error)),
        }
    }
}

/// Owner control socket for the remote node ingress server.
pub fn remote_node_ingress_owner_addr(network: &RemoteNetworkConfig) -> RemoteControlAddr {
    remote_control_addr(network, RemoteControlKind::IngressOwner)
}

/// Owner control socket for the remote runtime owner sidecar.
pub fn remote_runtime_owner_addr(network: &RemoteNetworkConfig) -> RemoteControlAddr {
    remote_control_addr(network, RemoteControlKind::RuntimeOwner)
}

/// Ephemeral one-shot address for parent/child ready handshakes.
///
/// Unix: a unique socket file under the temp dir. Windows: TCP loopback on
/// port 0; the concrete port is resolved by [`RemoteControlListener::local_addr`]
/// after binding.
pub fn remote_ready_addr() -> RemoteControlAddr {
    #[cfg(unix)]
    {
        static READY_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = READY_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        RemoteControlAddr::Unix(
            std::env::temp_dir().join(format!("waitagent-ready-{}-{seq}.sock", std::process::id())),
        )
    }
    #[cfg(windows)]
    {
        RemoteControlAddr::Tcp(std::net::SocketAddr::from(([127, 0, 0, 1], 0)))
    }
}

#[derive(Debug, Clone, Copy)]
enum RemoteControlKind {
    IngressOwner,
    RuntimeOwner,
}

impl RemoteControlKind {
    #[cfg(unix)]
    fn unix_file_stem(self) -> &'static str {
        match self {
            RemoteControlKind::IngressOwner => "waitagent-remote-node-ingress-",
            RemoteControlKind::RuntimeOwner => "waitagent-remote-runtime-owner-",
        }
    }

    #[cfg(windows)]
    fn windows_port_offset(self) -> u16 {
        match self {
            RemoteControlKind::IngressOwner => 10_000,
            RemoteControlKind::RuntimeOwner => 10_001,
        }
    }
}

#[cfg(unix)]
fn remote_control_addr(
    network: &RemoteNetworkConfig,
    kind: RemoteControlKind,
) -> RemoteControlAddr {
    let file_name = format!(
        "{}{}.sock",
        kind.unix_file_stem(),
        sanitize(&network.listener_addr().to_string())
    );
    RemoteControlAddr::Unix(std::env::temp_dir().join(file_name))
}

#[cfg(windows)]
fn remote_control_addr(
    network: &RemoteNetworkConfig,
    kind: RemoteControlKind,
) -> RemoteControlAddr {
    let port = network.port.saturating_add(kind.windows_port_offset());
    RemoteControlAddr::Tcp(std::net::SocketAddr::from(([127, 0, 0, 1], port)))
}

/// Startup lock file for the ingress owner control socket.
///
/// The path is identical on all platforms: a regular file in the temp dir
/// that [`crate::platform::file_lock::StartupLock`] locks.
pub fn remote_node_ingress_startup_lock_path(network: &RemoteNetworkConfig) -> PathBuf {
    startup_lock_path("waitagent-remote-node-ingress-", network)
}

/// Startup lock file for the remote runtime owner control socket.
pub fn remote_runtime_owner_startup_lock_path(network: &RemoteNetworkConfig) -> PathBuf {
    startup_lock_path("waitagent-remote-runtime-owner-", network)
}

/// Owner control socket for the remote session sync sidecar.
///
/// Keyed by socket name (not network). The Unix file name must stay identical
/// to the historical name; the sanitize alphabet keeps `-` and `_`.
pub fn remote_session_sync_owner_addr(socket_name: &str) -> RemoteControlAddr {
    #[cfg(unix)]
    {
        let name = sanitize_socket_name(socket_name);
        RemoteControlAddr::Unix(
            std::env::temp_dir().join(format!("waitagent-remote-session-sync-owner-{name}.sock")),
        )
    }
    #[cfg(windows)]
    {
        // No network port is available here; derive a stable loopback-only port
        // from the socket name.
        let port = 40_000 + (fnv1a_64(socket_name.as_bytes()) % 20_000) as u16;
        RemoteControlAddr::Tcp(std::net::SocketAddr::from(([127, 0, 0, 1], port)))
    }
}

/// Startup lock file for the session sync owner control socket.
pub fn remote_session_sync_startup_lock_path(socket_name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "waitagent-remote-session-sync-owner-{}.sock.lock",
        sanitize_socket_name(socket_name)
    ))
}

fn sanitize_socket_name(value: &str) -> String {
    value
        .chars()
        .map(|ch| match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' => ch,
            _ => '_',
        })
        .collect()
}

#[cfg(windows)]
fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn startup_lock_path(file_stem: &str, network: &RemoteNetworkConfig) -> PathBuf {
    std::env::temp_dir().join(format!(
        "{}{}.sock.lock",
        file_stem,
        sanitize(&network.listener_addr().to_string())
    ))
}

fn sanitize(value: &str) -> String {
    value
        .chars()
        .map(|ch| match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' => ch,
            _ => '_',
        })
        .collect()
}

/// Returns true if a remote-control listener appears to be active.
pub fn remote_listener_is_running(addr: &RemoteControlAddr) -> bool {
    match addr {
        #[cfg(unix)]
        RemoteControlAddr::Unix(path) => {
            path.exists() && std::os::unix::net::UnixStream::connect(path).is_ok()
        }
        #[cfg(windows)]
        RemoteControlAddr::Tcp(addr) => {
            std::net::TcpStream::connect_timeout(addr, std::time::Duration::from_millis(200))
                .is_ok()
        }
    }
}

/// A connected remote-control stream.
pub struct RemoteControlStream {
    #[cfg(unix)]
    inner: std::os::unix::net::UnixStream,
    #[cfg(windows)]
    inner: std::net::TcpStream,
}

impl RemoteControlStream {
    pub fn connect(addr: &RemoteControlAddr) -> io::Result<Self> {
        match addr {
            #[cfg(unix)]
            RemoteControlAddr::Unix(path) => {
                let inner = std::os::unix::net::UnixStream::connect(path)?;
                Ok(Self { inner })
            }
            #[cfg(windows)]
            RemoteControlAddr::Tcp(tcp_addr) => {
                let inner = std::net::TcpStream::connect_timeout(
                    tcp_addr,
                    std::time::Duration::from_secs(2),
                )?;
                let _ = inner.set_nodelay(true);
                Ok(Self { inner })
            }
        }
    }

    pub fn set_read_timeout(&self, dur: Option<std::time::Duration>) -> io::Result<()> {
        self.inner.set_read_timeout(dur)
    }

    pub fn set_write_timeout(&self, dur: Option<std::time::Duration>) -> io::Result<()> {
        self.inner.set_write_timeout(dur)
    }

    pub fn shutdown(&self, how: std::net::Shutdown) -> io::Result<()> {
        self.inner.shutdown(how)
    }

    pub fn try_clone(&self) -> io::Result<Self> {
        let inner = self.inner.try_clone()?;
        Ok(Self { inner })
    }

    #[cfg(windows)]
    fn tune_accepted(stream: &std::net::TcpStream) {
        let _ = stream.set_nodelay(true);
    }
}

/// A pair of connected in-process streams (bidirectional pipe).
///
/// Unix uses `UnixStream::pair()`. Windows uses a transient TCP loopback
/// connection because `UnixStream` is unavailable there.
pub fn socket_pair() -> io::Result<(RemoteControlStream, RemoteControlStream)> {
    #[cfg(unix)]
    {
        let (a, b) = std::os::unix::net::UnixStream::pair()?;
        Ok((
            RemoteControlStream { inner: a },
            RemoteControlStream { inner: b },
        ))
    }
    #[cfg(windows)]
    {
        use std::net::{Ipv4Addr, TcpListener, TcpStream};
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let addr = listener.local_addr()?;
        let a = TcpStream::connect(addr)?;
        let (b, _) = listener.accept()?;
        let _ = a.set_nodelay(true);
        let _ = b.set_nodelay(true);
        Ok((
            RemoteControlStream { inner: a },
            RemoteControlStream { inner: b },
        ))
    }
}

impl io::Read for RemoteControlStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buf)
    }
}

impl io::Write for RemoteControlStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// A remote-control listener (blocking).
pub struct RemoteControlListener {
    #[cfg(unix)]
    inner: std::os::unix::net::UnixListener,
    #[cfg(windows)]
    inner: std::net::TcpListener,
    addr: RemoteControlAddr,
}

impl RemoteControlListener {
    pub fn bind(addr: &RemoteControlAddr) -> io::Result<Self> {
        match addr {
            #[cfg(unix)]
            RemoteControlAddr::Unix(path) => {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                crate::infra::best_effort::remove_file(path);
                let inner = std::os::unix::net::UnixListener::bind(path)?;
                Ok(Self {
                    inner,
                    addr: addr.clone(),
                })
            }
            #[cfg(windows)]
            RemoteControlAddr::Tcp(tcp_addr) => {
                let inner = std::net::TcpListener::bind(*tcp_addr)?;
                let resolved = RemoteControlAddr::Tcp(inner.local_addr()?);
                Ok(Self {
                    inner,
                    addr: resolved,
                })
            }
        }
    }

    pub fn accept(&self) -> io::Result<(RemoteControlStream, RemoteControlAddr)> {
        match &self.addr {
            #[cfg(unix)]
            RemoteControlAddr::Unix(_) => {
                let (stream, _) = self.inner.accept()?;
                Ok((RemoteControlStream { inner: stream }, self.addr.clone()))
            }
            #[cfg(windows)]
            RemoteControlAddr::Tcp(_) => {
                let (stream, _) = self.inner.accept()?;
                RemoteControlStream::tune_accepted(&stream);
                Ok((RemoteControlStream { inner: stream }, self.addr.clone()))
            }
        }
    }

    /// The resolved address: on Windows this is the concrete port for
    /// listeners bound on port 0.
    pub fn local_addr(&self) -> &RemoteControlAddr {
        &self.addr
    }
}

/// A tokio-based remote-control listener.
pub struct RemoteControlAsyncListener {
    #[cfg(unix)]
    inner: tokio::net::UnixListener,
    #[cfg(windows)]
    inner: tokio::net::TcpListener,
    addr: RemoteControlAddr,
}

impl RemoteControlAsyncListener {
    pub async fn bind(addr: &RemoteControlAddr) -> io::Result<Self> {
        match addr {
            #[cfg(unix)]
            RemoteControlAddr::Unix(path) => {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                crate::infra::best_effort::remove_file(path);
                let inner = tokio::net::UnixListener::bind(path)?;
                Ok(Self {
                    inner,
                    addr: addr.clone(),
                })
            }
            #[cfg(windows)]
            RemoteControlAddr::Tcp(tcp_addr) => {
                let inner = tokio::net::TcpListener::bind(*tcp_addr).await?;
                let resolved = RemoteControlAddr::Tcp(inner.local_addr()?);
                Ok(Self {
                    inner,
                    addr: resolved,
                })
            }
        }
    }

    pub async fn accept(&self) -> io::Result<(RemoteControlAsyncStream, RemoteControlAddr)> {
        match &self.addr {
            #[cfg(unix)]
            RemoteControlAddr::Unix(_) => {
                let (stream, _) = self.inner.accept().await?;
                Ok((
                    RemoteControlAsyncStream { inner: stream },
                    self.addr.clone(),
                ))
            }
            #[cfg(windows)]
            RemoteControlAddr::Tcp(_) => {
                let (stream, _) = self.inner.accept().await?;
                let _ = stream.set_nodelay(true);
                Ok((
                    RemoteControlAsyncStream { inner: stream },
                    self.addr.clone(),
                ))
            }
        }
    }
}

/// A tokio-based connected remote-control stream.
pub struct RemoteControlAsyncStream {
    #[cfg(unix)]
    inner: tokio::net::UnixStream,
    #[cfg(windows)]
    inner: tokio::net::TcpStream,
}

impl tokio::io::AsyncRead for RemoteControlAsyncStream {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_read(cx, buf)
    }
}

impl tokio::io::AsyncWrite for RemoteControlAsyncStream {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<io::Result<usize>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

/// Remove any on-disk artifacts for the address, if applicable.
pub fn cleanup_remote_listener(addr: &RemoteControlAddr) {
    #[cfg(unix)]
    if let Some(path) = addr.unix_path() {
        crate::infra::best_effort::remove_file(path);
    }
    #[cfg(windows)]
    let _ = addr;
}

// ---------------------------------------------------------------------------
// Authority transport endpoints
//
// Cross-process authority transport bridges. Unix uses hashed UDS file names
// under the temp dir. Windows uses TCP loopback with a `<stem>.port` marker
// file per endpoint (same discovery shape as the UDS scan).
// ---------------------------------------------------------------------------

/// FNV-1a 64 hash, hex-encoded. Shared by endpoint naming and discovery so
/// file names stay identical across the codebase.
pub fn stable_socket_hash(values: &[&str]) -> String {
    let mut hash = 0xcbf29ce484222325_u64;
    for value in values {
        for byte in value.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
    }
    format!("{hash:016x}")
}

/// Address of an authority transport endpoint.
///
/// Unix: the historical hashed UDS path. Windows: TCP loopback on an
/// ephemeral port; the concrete port is published via
/// [`authority_transport_marker_path`].
pub fn authority_transport_addr(
    socket_name: &str,
    session_name: &str,
    target: &str,
) -> RemoteControlAddr {
    let scope_hash = stable_socket_hash(&[socket_name, session_name]);
    let authority_hash = stable_socket_hash(&[target_authority_id(target)]);
    let target_hash = target_session_component(target);
    let stem = format!("waitagent-remote-{scope_hash}-{authority_hash}-{target_hash}");
    #[cfg(unix)]
    {
        RemoteControlAddr::Unix(std::env::temp_dir().join(format!("{stem}.sock")))
    }
    #[cfg(windows)]
    {
        let _ = stem;
        RemoteControlAddr::Tcp(std::net::SocketAddr::from(([127, 0, 0, 1], 0)))
    }
}

/// Marker file publishing the TCP port of an authority endpoint (Windows only
/// consumer; harmless on Unix). Same file-name stem as the UDS socket so
/// discovery and target-hash extraction work identically on both platforms.
pub fn authority_transport_marker_path(
    socket_name: &str,
    session_name: &str,
    target: &str,
) -> PathBuf {
    let scope_hash = stable_socket_hash(&[socket_name, session_name]);
    let authority_hash = stable_socket_hash(&[target_authority_id(target)]);
    let target_hash = target_session_component(target);
    std::env::temp_dir().join(format!(
        "waitagent-remote-{scope_hash}-{authority_hash}-{target_hash}.port"
    ))
}

fn target_authority_id(target: &str) -> &str {
    split_target_identity(target)
        .map(|(authority_id, _)| authority_id)
        .unwrap_or(target)
}

fn target_session_component(target: &str) -> String {
    split_target_identity(target)
        .map(|(authority_id, session_id)| stable_socket_hash(&[authority_id, ":", session_id]))
        .unwrap_or_else(|| stable_socket_hash(&[target]))
}

fn split_target_identity(target: &str) -> Option<(&str, &str)> {
    let target = target
        .strip_prefix("remote-peer:")
        .or_else(|| target.strip_prefix("local-tmux:"))
        .or_else(|| target.strip_prefix("local:"))
        .or_else(|| target.strip_prefix("remote:"))
        .unwrap_or(target);
    let (authority_id, session_id) = target.rsplit_once(':')?;
    if authority_id.is_empty() || session_id.is_empty() {
        return None;
    }
    Some((authority_id, session_id))
}

/// A bound authority transport endpoint.
pub struct AuthorityEndpointListener {
    listener: RemoteControlListener,
    /// Marker file written on Windows; removed on drop.
    #[cfg(windows)]
    marker: PathBuf,
}

/// Bind an authority endpoint. On Windows this also writes the marker file
/// publishing the resolved TCP port, and removes it (plus the Unix socket
/// file) on drop.
pub fn bind_authority_endpoint(
    addr: &RemoteControlAddr,
    marker: &Path,
) -> io::Result<AuthorityEndpointListener> {
    let listener = RemoteControlListener::bind(addr)?;
    #[cfg(windows)]
    {
        if let Some(parent) = marker.parent() {
            std::fs::create_dir_all(parent)?;
        }
        crate::infra::best_effort::remove_file(marker);
        let port = match listener.local_addr() {
            RemoteControlAddr::Tcp(tcp_addr) => tcp_addr.port(),
        };
        std::fs::write(marker, format!("{port}\n"))?;
    }
    #[cfg(unix)]
    let _ = marker;
    Ok(AuthorityEndpointListener {
        listener,
        #[cfg(windows)]
        marker: marker.to_path_buf(),
    })
}

impl AuthorityEndpointListener {
    pub fn accept(&self) -> io::Result<(RemoteControlStream, RemoteControlAddr)> {
        self.listener.accept()
    }

    pub fn local_addr(&self) -> &RemoteControlAddr {
        self.listener.local_addr()
    }
}

impl Drop for AuthorityEndpointListener {
    fn drop(&mut self) {
        #[cfg(windows)]
        crate::infra::best_effort::remove_file(&self.marker);
        cleanup_remote_listener(self.listener.local_addr());
    }
}

/// Serializes an authority endpoint for registration with the ingress owner.
///
/// Unix: the socket path (historical wire format). Windows: the marker file
/// path (the peer resolves the port from it).
pub fn authority_endpoint_id_string(addr: &RemoteControlAddr, marker: &Path) -> String {
    #[cfg(unix)]
    let _ = marker;
    match addr {
        #[cfg(unix)]
        RemoteControlAddr::Unix(path) => path.display().to_string(),
        #[cfg(windows)]
        RemoteControlAddr::Tcp(_) => marker.display().to_string(),
    }
}

/// Parses an endpoint id string produced by [`authority_endpoint_id_string`].
///
/// Returns the connect address plus the id file whose name carries the
/// target hash (socket path on Unix, marker file on Windows).
pub fn authority_endpoint_from_id_string(value: &str) -> io::Result<(RemoteControlAddr, PathBuf)> {
    #[cfg(unix)]
    {
        let path = PathBuf::from(value);
        Ok((RemoteControlAddr::Unix(path.clone()), path))
    }
    #[cfg(windows)]
    {
        let marker = PathBuf::from(value);
        let port: u16 = std::fs::read_to_string(&marker)?
            .trim()
            .parse()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        Ok((
            RemoteControlAddr::Tcp(std::net::SocketAddr::from(([127, 0, 0, 1], port))),
            marker,
        ))
    }
}

/// Scan the temp dir for live authority endpoints owned by `authority_id`.
///
/// Each entry is `(connect addr, id file)`: the id file's name carries the
/// target hash and is used both for target extraction and as the on-disk
/// identity during refresh.
pub fn discover_authority_endpoints(
    authority_id: &str,
) -> io::Result<Vec<(RemoteControlAddr, PathBuf)>> {
    let authority_hash = stable_socket_hash(&[authority_id]);
    let mut endpoints = Vec::new();
    for entry in std::fs::read_dir(std::env::temp_dir())? {
        let entry = entry?;
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy();
        #[cfg(unix)]
        let extension = ".sock";
        #[cfg(windows)]
        let extension = ".port";
        if !name.starts_with("waitagent-remote-") || !name.ends_with(extension) {
            continue;
        }
        if !name.contains(&format!("-{authority_hash}-")) {
            continue;
        }
        let path = entry.path();
        #[cfg(unix)]
        endpoints.push((RemoteControlAddr::Unix(path.clone()), path));
        #[cfg(windows)]
        {
            let Ok(port) = std::fs::read_to_string(&path)?.trim().parse::<u16>() else {
                continue;
            };
            endpoints.push((
                RemoteControlAddr::Tcp(std::net::SocketAddr::from(([127, 0, 0, 1], port))),
                path,
            ));
        }
    }
    Ok(endpoints)
}
