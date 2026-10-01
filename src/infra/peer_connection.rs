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

/// Connect timeout preserved verbatim from the pre-seam direct TCP dial in
/// `remote_grpc_transport::TlsPinConnector::call`.
const TCP_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// An ordered, reliable, full-duplex byte stream between two nodes — TCP
/// semantics. This is the transport boundary below the node-to-node inner
/// TLS: implementations must deliver the written byte sequence in order,
/// without loss, in both directions.
pub trait PeerConnection: AsyncRead + AsyncWrite + Send + Unpin + 'static {}

impl<T> PeerConnection for T where T: AsyncRead + AsyncWrite + Send + Unpin + 'static {}

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
}
