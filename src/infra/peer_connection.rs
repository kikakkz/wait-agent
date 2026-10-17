//! PeerConnection transport seam.
//!
//! Governing design: `docs/relay-design.md` (协议分层 / 集成 seam). The
//! node-to-node inner TLS and all session protocols (AuthorityTransport,
//! ControlPlane, ClientHello, ...) run **above** this seam and stay
//! transport-unaware. Direct TCP is the phase-1 implementation; the relay
//! mux stream (issue #51) plugs in as a second implementation, which is why
//! the dial returns a boxed trait object.

use std::io;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite};

use crate::infra::proxy_stream::{open_proxy_stream, SshProxy};

/// Connect timeout preserved verbatim from the pre-seam direct TCP dial in
/// `remote_grpc_transport::TlsPinConnector::call`.
const TCP_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// An ordered, reliable, full-duplex byte stream between two nodes — TCP
/// semantics. This is the transport boundary below the node-to-node inner
/// TLS: implementations must deliver the written byte sequence in order,
/// without loss, in both directions.
pub trait PeerConnection: AsyncRead + AsyncWrite + Send + Unpin + 'static {}

impl<T> PeerConnection for T where T: AsyncRead + AsyncWrite + Send + Unpin + 'static {}

/// Dials a peer connection to `host:port`, through `proxy` when one is
/// given (issue #206: the relay join and the persistent relay link reuse
/// the SSH-side proxy resolution). A direct dial is byte-identical to
/// [`dial_tcp_peer_connection`]; the proxied path hands the pre-connected
/// proxy stream (a plain `tokio::net::TcpStream`, which already satisfies
/// the [`PeerConnection`] bound) to the same boxing.
pub async fn dial_peer_connection(
    proxy: Option<&SshProxy>,
    host: &str,
    port: u16,
) -> io::Result<Box<dyn PeerConnection>> {
    match proxy {
        Some(proxy) => {
            let stream =
                tokio::time::timeout(TCP_CONNECT_TIMEOUT, open_proxy_stream(proxy, host, port))
                    .await
                    .map_err(|_| {
                        io::Error::new(
                            io::ErrorKind::TimedOut,
                            format!("proxy tcp connect to {} timed out", proxy.label()),
                        )
                    })?
                    .map_err(|error| {
                        io::Error::other(format!(
                            "dial via proxy {} to {host}:{port} failed: {error}",
                            proxy.label()
                        ))
                    })?;
            Ok(Box::new(stream))
        }
        None => dial_tcp_peer_connection(host, port).await,
    }
}

/// Dials a direct TCP peer connection to `host:port`.
///
/// Byte-identical to the TCP dial that used to live inline in
/// `TlsPinConnector::call`: same 30s connect timeout, same timeout error
/// (kept verbatim, log output is observable behavior), same refusal
/// propagation. No socket options are applied here — the pre-seam dial
/// applied none on the custom-connector path.
pub async fn dial_tcp_peer_connection(
    host: &str,
    port: u16,
) -> io::Result<Box<dyn PeerConnection>> {
    match tokio::time::timeout(
        TCP_CONNECT_TIMEOUT,
        tokio::net::TcpStream::connect((host, port)),
    )
    .await
    {
        Ok(Ok(stream)) => Ok(Box::new(stream)),
        Ok(Err(error)) => Err(error),
        Err(_) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "tls-pin tcp connect timed out",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn dials_loopback_and_exchanges_bytes_in_order() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("local addr");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let mut buf = [0u8; 8];
            stream.read_exact(&mut buf).await.expect("server read");
            stream.write_all(&buf).await.expect("server write");
        });

        let mut conn = dial_tcp_peer_connection("127.0.0.1", addr.port())
            .await
            .expect("dial");
        conn.write_all(b"pingpong").await.expect("client write");
        let mut buf = [0u8; 8];
        conn.read_exact(&mut buf).await.expect("client read");
        assert_eq!(&buf, b"pingpong");
        server.await.expect("server task");
    }

    #[tokio::test]
    async fn propagates_connection_refused() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("local addr");
        drop(listener);

        let error = match dial_tcp_peer_connection("127.0.0.1", addr.port()).await {
            Ok(_) => panic!("dial to a closed port should fail"),
            Err(error) => error,
        };
        assert_eq!(
            error.kind(),
            io::ErrorKind::ConnectionRefused,
            "refusal should propagate unchanged, got: {error}"
        );
    }

    /// Issue #206: a proxied dial reaches the target through a real SOCKS5
    /// handshake and delivers written bytes end to end.
    #[tokio::test]
    async fn dial_peer_connection_through_socks5_reaches_the_target() {
        // Echo target behind the proxy.
        let target = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let target_addr = target.local_addr().expect("local addr");
        let echo = tokio::spawn(async move {
            let (mut stream, _) = target.accept().await.expect("accept");
            let mut buf = [0u8; 8];
            stream.read_exact(&mut buf).await.expect("server read");
            stream.write_all(&buf).await.expect("server write");
        });

        // SOCKS5 no-auth mock that actually connects to the requested target
        // and pipes both directions (same shape as proxy_stream's mocks, but
        // forwarding instead of terminating).
        let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let proxy_addr = proxy_listener.local_addr().expect("local addr");
        let proxy = tokio::spawn(async move {
            let (mut stream, _) = proxy_listener.accept().await.expect("accept");
            let mut greeting = [0_u8; 3];
            stream.read_exact(&mut greeting).await.expect("greeting");
            stream.write_all(&[0x05, 0x00]).await.expect("method");
            let mut head = [0_u8; 5];
            stream.read_exact(&mut head).await.expect("head");
            let len = usize::from(head[4]);
            let mut body = vec![0_u8; len + 2];
            stream.read_exact(&mut body).await.expect("body");
            let port = u16::from_be_bytes([body[len], body[len + 1]]);
            let upstream = tokio::net::TcpStream::connect((
                String::from_utf8_lossy(&body[..len]).as_ref(),
                port,
            ))
            .await
            .expect("mock upstream connect");
            stream
                .write_all(&[0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
                .await
                .expect("reply");
            let mut upstream = upstream;
            tokio::io::copy_bidirectional(&mut stream, &mut upstream)
                .await
                .expect("pipe");
        });

        let proxy_config = SshProxy {
            scheme: crate::infra::proxy_stream::SshProxyScheme::Socks5,
            host: "127.0.0.1".to_string(),
            port: proxy_addr.port(),
        };
        let mut conn = dial_peer_connection(Some(&proxy_config), "127.0.0.1", target_addr.port())
            .await
            .expect("proxied dial");
        conn.write_all(b"pingpong").await.expect("client write");
        let mut buf = [0u8; 8];
        conn.read_exact(&mut buf).await.expect("client read");
        assert_eq!(&buf, b"pingpong");
        echo.await.expect("echo task");
        proxy.abort();
    }

    /// Issue #206: without a proxy the variadic dial is exactly the direct
    /// dial, refusal included.
    #[tokio::test]
    async fn dial_peer_connection_without_proxy_matches_direct_refusal() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("local addr");
        drop(listener);

        let error = match dial_peer_connection(None, "127.0.0.1", addr.port()).await {
            Ok(_) => panic!("dial to a closed port should fail"),
            Err(error) => error,
        };
        assert_eq!(
            error.kind(),
            io::ErrorKind::ConnectionRefused,
            "refusal should propagate unchanged, got: {error}"
        );
    }
}
