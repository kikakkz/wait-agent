//! Relay-routed inbound streams for the node ingress listener (issue #129).
//!
//! When the node enrolls a relay, peers can dial in with `via = "relay"`;
//! those streams surface on the enrolled relay client's `accept_inbound`
//! queue. This module feeds every accepted stream into the ingress
//! listener's tonic server so it travels the identical server-side pipeline
//! as a direct TCP accept (tonic server TLS, ClientHello, operator auth) and
//! its events land in the same `RemoteNodeTransportEvent` stream — the
//! ingress loop sees no difference between the two accept paths.
//!
//! Lock order (m07): the accept worker holds no project locks —
//! `accept_inbound` waits on the relay client's internal inbound queue
//! mutex, connections move to the tonic worker only through channel sends,
//! and no event is ever sent from inside a lock. The ingress loop remains
//! the sole `SharedState` writer.

use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::thread;

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::mpsc as tokio_mpsc;
use tonic::transport::server::Connected;

use crate::infra::error_log::ERROR_LOG;
use crate::infra::peer_connection::PeerConnection;
use crate::infra::relay_client::RelayClientHandle;

/// Connection handed to the ingress listener's tonic server. A relay-routed
/// inbound stream presents the exact same IO surface as an accepted TCP
/// connection, so both variants go through the same server-side handling.
pub(crate) enum NodeIngressIo {
    Tcp(tokio::net::TcpStream),
    Relay(Box<dyn PeerConnection>),
}

/// Connection metadata tonic stores in request extensions. Relay-routed
/// streams carry no socket address: the relay does not expose the dialer's.
/// Nothing in the ingress service reads the extension yet; the field is
/// kept to mirror tonic's `TcpConnectInfo`.
#[derive(Debug, Clone)]
pub(crate) struct NodeIngressConnectInfo {
    #[allow(dead_code)]
    pub(crate) peer_addr: Option<SocketAddr>,
}

impl Connected for NodeIngressIo {
    type ConnectInfo = NodeIngressConnectInfo;

    fn connect_info(&self) -> Self::ConnectInfo {
        match self {
            Self::Tcp(stream) => NodeIngressConnectInfo {
                peer_addr: stream.peer_addr().ok(),
            },
            Self::Relay(_) => NodeIngressConnectInfo { peer_addr: None },
        }
    }
}

impl AsyncRead for NodeIngressIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match &mut *self {
            Self::Tcp(stream) => Pin::new(stream).poll_read(cx, buf),
            Self::Relay(connection) => Pin::new(&mut **connection).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for NodeIngressIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match &mut *self {
            Self::Tcp(stream) => Pin::new(stream).poll_write(cx, buf),
            Self::Relay(connection) => Pin::new(&mut **connection).poll_write(cx, buf),
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut *self {
            Self::Tcp(stream) => Pin::new(stream).poll_flush(cx),
            Self::Relay(connection) => Pin::new(&mut **connection).poll_flush(cx),
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut *self {
            Self::Tcp(stream) => Pin::new(stream).poll_shutdown(cx),
            Self::Relay(connection) => Pin::new(&mut **connection).poll_shutdown(cx),
        }
    }
}

/// Spawns the relay inbound accept worker: loops
/// [`RelayClientHandle::accept_inbound`] and hands every relay-routed stream
/// to the ingress listener's tonic worker through `relay_tx`. Exits when
/// `accept_inbound` returns `None` (relay client stopped), when the listener
/// is gone (the send fails), or once `stop` is set at the latest wakeup.
///
/// Shutdown is signal-only, exactly like the TCP accept worker's: the
/// transport guard sets `stop` in its `Drop` and never joins this thread —
/// `accept_inbound` parks until a stream arrives or the relay client stops,
/// and process exit tears the worker down regardless. The worker's handle
/// clone keeps the relay client alive exactly like an in-flight dial guard
/// does; its drop cancels later.
pub(crate) fn spawn_relay_accept_worker(
    relay_client: Arc<RelayClientHandle>,
    relay_tx: tokio_mpsc::UnboundedSender<NodeIngressIo>,
    stop: Arc<AtomicBool>,
) -> io::Result<thread::JoinHandle<()>> {
    thread::Builder::new()
        .name("remote-ingress-relay-accept".to_string())
        .spawn(move || loop {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            let Some(connection) = relay_client.accept_inbound() else {
                return;
            };
            if stop.load(Ordering::Relaxed) {
                return;
            }
            if relay_tx.send(NodeIngressIo::Relay(connection)).is_err() {
                ERROR_LOG.log_error(
                    "relay ingress accept worker: ingress listener gone; stopping".to_string(),
                );
                return;
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn relay_variant_delegates_io_and_reports_no_peer_addr() {
        let (client, mut server) = tokio::io::duplex(64);
        let mut io = NodeIngressIo::Relay(Box::new(client));
        assert!(
            io.connect_info().peer_addr.is_none(),
            "relay-routed streams carry no socket address"
        );

        io.write_all(b"ping").await.expect("write should delegate");
        let mut buf = [0u8; 4];
        server
            .read_exact(&mut buf)
            .await
            .expect("peer should receive the delegated write");
        assert_eq!(&buf, b"ping");
        server.write_all(b"pong").await.expect("peer should reply");
        let mut reply = [0u8; 4];
        io.read_exact(&mut reply)
            .await
            .expect("read should delegate");
        assert_eq!(&reply, b"pong");
    }
}
