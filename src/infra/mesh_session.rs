//! Mesh session data plane: QUIC streams between enrolled nodes (issue #268; see docs/coordination-design.md).

use std::io;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};

use iroh::{Endpoint, EndpointAddr, PublicKey, RelayMap, RelayMode, SecretKey, TransportAddr};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::mpsc as tokio_mpsc;

use crate::infra::peer_connection::PeerConnection;
use crate::infra::relay_ingress::NodeIngressIo;

pub const MESH_SESSION_ALPN: &[u8] = b"waitagent/mesh-session/0";

pub fn node_key_path(home: &std::path::Path) -> PathBuf {
    home.join("coordination-node.key")
}

pub fn saved_coordination_server_path(home: &std::path::Path) -> PathBuf {
    home.join("coordination-server.json")
}

pub fn saved_relay_path(home: &std::path::Path) -> PathBuf {
    home.join("coordination-relay.json")
}

fn waitagent_home() -> PathBuf {
    crate::host::ssh::remote_host_home::waitagent_home()
}

pub fn load_node_key_at(home: &std::path::Path) -> Result<SecretKey, String> {
    crate::infra::coordination::enrollment::load_or_generate_key(&node_key_path(home))
        .map_err(|error| format!("load mesh node key: {error}"))
}

fn saved_relay_url(home: &std::path::Path) -> Option<String> {
    std::fs::read_to_string(saved_relay_path(home))
        .ok()
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
}

async fn build_node_endpoint_at(
    key: SecretKey,
    home: &std::path::Path,
) -> Result<Endpoint, String> {
    let relay_mode = match saved_relay_url(home) {
        Some(url) => {
            let map = RelayMap::try_from_iter([url.as_str()])
                .map_err(|error| format!("invalid saved relay url {url:?}: {error}"))?;
            RelayMode::Custom(map)
        }
        None => RelayMode::Disabled,
    };
    Endpoint::builder(iroh::endpoint::presets::Minimal)
        .relay_mode(relay_mode)
        .secret_key(key)
        .alpns(vec![MESH_SESSION_ALPN.to_vec()])
        .bind()
        .await
        .map_err(|error| format!("bind mesh endpoint: {error}"))
}

/// One session's QUIC stream. Owns an endpoint handle so the transport
/// outlives the stream exactly.
pub struct MeshStream {
    send: iroh::endpoint::SendStream,
    recv: iroh::endpoint::RecvStream,
    _endpoint: Arc<Endpoint>,
}

impl AsyncRead for MeshStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        AsyncRead::poll_read(Pin::new(&mut self.recv), cx, buf)
    }
}

impl AsyncWrite for MeshStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        AsyncWrite::poll_write(Pin::new(&mut self.send), cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        AsyncWrite::poll_flush(Pin::new(&mut self.send), cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        AsyncWrite::poll_shutdown(Pin::new(&mut self.send), cx)
    }
}

async fn round_trip(
    key: SecretKey,
    server: EndpointAddr,
    alpn: &[u8],
    frame: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let endpoint = Endpoint::builder(iroh::endpoint::presets::Minimal)
        .relay_mode(RelayMode::Disabled)
        .secret_key(key)
        .alpns(vec![alpn.to_vec()])
        .bind()
        .await
        .map_err(|error| format!("bind resolve endpoint: {error}"))?;
    let conn = endpoint
        .connect(server, alpn)
        .await
        .map_err(|error| format!("connect coordination server: {error}"))?;
    let (mut send, mut recv) = conn
        .open_bi()
        .await
        .map_err(|error| format!("open resolve stream: {error}"))?;
    let mut bytes = serde_json::to_vec(frame).map_err(|error| error.to_string())?;
    bytes.push(b'\n');
    tokio::io::AsyncWriteExt::write_all(&mut send, &bytes)
        .await
        .map_err(|error| format!("write resolve frame: {error}"))?;
    let reply = recv
        .read_to_end(1024 * 1024)
        .await
        .map_err(|error| format!("read resolve reply: {error}"))?;
    endpoint.close().await;
    serde_json::from_slice(&reply).map_err(|error| format!("invalid resolve reply: {error}"))
}

/// Resolves the target's last-known NodeAddr through the remembered
/// coordination server; unregistered targets fail with the server's message.
pub async fn resolve_mesh_node_addr_at(
    target_node_id: &str,
    home: &std::path::Path,
) -> Result<EndpointAddr, String> {
    target_node_id
        .parse::<PublicKey>()
        .map_err(|error| format!("invalid mesh node id {target_node_id:?}: {error}"))?;
    let server_text =
        std::fs::read_to_string(saved_coordination_server_path(home)).map_err(|_| {
            "no remembered coordination server; run coordination enroll-request first".to_string()
        })?;
    let server = crate::infra::mesh_probe::parse_node_addr(&server_text)?;
    let key = load_node_key_at(home)?;
    let frame = serde_json::json!({"command": "resolve", "node_id": target_node_id});
    let reply = round_trip(key, server, crate::infra::coordination::NODE_ALPN, &frame).await?;
    if reply.get("ok") != Some(&serde_json::Value::Bool(true)) {
        return Err(reply
            .get("error")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("coordination resolve failed")
            .to_string());
    }
    let addr_text = reply
        .get("node_addr")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "coordination resolve returned no node_addr".to_string())?;
    crate::infra::mesh_probe::parse_node_addr(addr_text)
}

/// Blocking resolve-and-dial for the pinned connector: builds a short-lived
/// runtime so it can run on the blocking thread pool (tonic polls the
/// connector in async context).
pub fn dial_mesh_peer_connection(target_node_id: &str) -> Result<Box<dyn PeerConnection>, String> {
    dial_mesh_peer_connection_at(target_node_id, &waitagent_home())
}

/// The dialer endpoint follows the resolved address's relay URL (Custom,
/// exactly that relay) and dials direct-first with the relay as fallback.
pub fn relay_mode_for_addr(addr: &EndpointAddr) -> Result<RelayMode, String> {
    match addr.addrs.iter().find_map(|entry| match entry {
        TransportAddr::Relay(url) => Some(url.to_string()),
        _ => None,
    }) {
        Some(url) => {
            let map = RelayMap::try_from_iter([url.as_str()])
                .map_err(|error| format!("invalid relay url {url:?}: {error}"))?;
            Ok(RelayMode::Custom(map))
        }
        None => Ok(RelayMode::Disabled),
    }
}

pub fn dial_mesh_peer_connection_at(
    target_node_id: &str,
    home: &std::path::Path,
) -> Result<Box<dyn PeerConnection>, String> {
    let target = target_node_id.to_string();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("build mesh dial runtime: {error}"))?;
    runtime.block_on(async move {
        let addr = resolve_mesh_node_addr_at(&target, home).await?;
        let key = load_node_key_at(home)?;
        let relay_mode = relay_mode_for_addr(&addr)?;
        let endpoint = Endpoint::builder(iroh::endpoint::presets::Minimal)
            .relay_mode(relay_mode)
            .secret_key(key)
            .alpns(vec![MESH_SESSION_ALPN.to_vec()])
            .bind()
            .await
            .map_err(|error| format!("bind mesh dial endpoint: {error}"))?;
        let conn = endpoint
            .connect(addr, MESH_SESSION_ALPN)
            .await
            .map_err(|error| format!("mesh connect to {target}: {error}"))?;
        let (send, recv) = conn
            .open_bi()
            .await
            .map_err(|error| format!("open mesh session stream: {error}"))?;
        Ok(Box::new(MeshStream {
            send,
            recv,
            _endpoint: Arc::new(endpoint),
        }) as Box<dyn PeerConnection>)
    })
}

/// Builds the node mesh endpoint on the enrolled key; the saved announced
/// relay URL selects the relay mode.
pub async fn build_node_mesh_endpoint_at(home: &std::path::Path) -> Result<Arc<Endpoint>, String> {
    let key = load_node_key_at(home)?;
    build_node_endpoint_at(key, home).await.map(Arc::new)
}

pub async fn build_node_mesh_endpoint() -> Result<Arc<Endpoint>, String> {
    build_node_mesh_endpoint_at(&waitagent_home()).await
}

/// Starts the mesh ingress (endpoint + accept worker) on dedicated threads,
/// so the call is legal from inside any runtime. The receiver yields every
/// accepted session stream.
pub fn start_mesh_ingress(
    stop: Arc<AtomicBool>,
) -> Result<tokio_mpsc::UnboundedReceiver<NodeIngressIo>, String> {
    let (mesh_tx, mesh_rx) = tokio_mpsc::unbounded_channel::<NodeIngressIo>();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
    std::thread::Builder::new()
        .name("remote-ingress-mesh".to_string())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    let _ = ready_tx.send(Err(format!("mesh ingress runtime: {error}")));
                    return;
                }
            };
            let started = runtime.block_on(async {
                let endpoint = build_node_mesh_endpoint().await?;
                spawn_mesh_accept_worker(endpoint, mesh_tx, stop).map_err(|error| error.to_string())
            });
            let _ = ready_tx.send(started.map(|_| ()));
        })
        .map_err(|error| format!("spawn mesh ingress thread: {error}"))?;
    ready_rx
        .recv()
        .map_err(|error| format!("mesh ingress start: {error}"))?
        .map(|_| mesh_rx)
}

/// Spawns the mesh inbound accept worker on the given endpoint and feeds
/// every accepted session stream into the ingress listener, mirroring the
/// legacy relay accept worker.
pub fn spawn_mesh_accept_worker(
    endpoint: Arc<Endpoint>,
    mesh_tx: tokio_mpsc::UnboundedSender<NodeIngressIo>,
    stop: Arc<AtomicBool>,
) -> io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("remote-ingress-mesh-accept".to_string())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    crate::infra::error_log::ERROR_LOG.log_error(format!(
                        "mesh accept worker: failed to build runtime: {error}"
                    ));
                    return;
                }
            };
            runtime.block_on(async move {
                loop {
                    if stop.load(Ordering::Relaxed) {
                        return;
                    }
                    let Some(incoming) = endpoint.accept().await else {
                        return;
                    };
                    let Ok(mut accepting) = incoming.accept() else {
                        continue;
                    };
                    let alpn = accepting.alpn().await.unwrap_or_default();
                    if alpn.as_slice() != MESH_SESSION_ALPN {
                        continue;
                    }
                    let Ok(conn) = accepting.await else {
                        continue;
                    };
                    let endpoint = endpoint.clone();
                    let mesh_tx = mesh_tx.clone();
                    tokio::spawn(async move {
                        let Ok((send, recv)) = conn.accept_bi().await else {
                            return;
                        };
                        if mesh_tx
                            .send(NodeIngressIo::Mesh(Box::new(MeshStream {
                                send,
                                recv,
                                _endpoint: endpoint,
                            })))
                            .is_err()
                        {
                            crate::infra::error_log::ERROR_LOG.log_error(
                                "mesh accept worker: ingress listener gone; stopping".to_string(),
                            );
                        }
                    });
                }
            });
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn temp_home(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "waitagent-mesh-{name}-{}-{}",
            std::process::id(),
            std::thread::current()
                .name()
                .unwrap_or("test")
                .replace(':', "_")
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn node_id_string() -> String {
        iroh::SecretKey::generate().public().to_string()
    }

    #[test]
    fn paths_and_the_enrolled_node_key_round_trip() {
        let home = temp_home("paths");
        let first = load_node_key_at(&home).unwrap();
        let second = load_node_key_at(&home).unwrap();
        assert_eq!(first.public(), second.public());
        assert!(node_key_path(&home).is_file());
        assert!(saved_coordination_server_path(&home).ends_with("coordination-server.json"));
        assert!(saved_relay_path(&home).ends_with("coordination-relay.json"));
        crate::infra::best_effort::remove_dir_all(&home);
    }

    #[tokio::test]
    async fn resolve_rejects_an_invalid_target_before_any_io() {
        let home = temp_home("invalid-target");
        let error = resolve_mesh_node_addr_at("bogus", &home)
            .await
            .expect_err("an invalid target id fails before any io");
        assert!(error.contains("invalid mesh node id"), "{error}");
        crate::infra::best_effort::remove_dir_all(&home);
    }

    #[tokio::test]
    async fn resolve_fails_without_a_remembered_server() {
        let home = temp_home("no-server");
        let error = resolve_mesh_node_addr_at(&node_id_string(), &home)
            .await
            .expect_err("no remembered coordination server is a clear error");
        assert!(error.contains("enroll-request"), "{error}");
        crate::infra::best_effort::remove_dir_all(&home);
    }

    #[tokio::test]
    async fn resolve_fails_on_a_garbage_server_addr() {
        let home = temp_home("garbage-server");
        std::fs::write(saved_coordination_server_path(&home), "garbage").unwrap();
        let error = resolve_mesh_node_addr_at(&node_id_string(), &home)
            .await
            .expect_err("a garbage remembered server addr is a clear error");
        assert!(error.contains("invalid node addr"), "{error}");
        crate::infra::best_effort::remove_dir_all(&home);
    }

    #[test]
    fn relay_mode_follows_the_resolved_addr() {
        let key = iroh::SecretKey::generate();
        let mut addr = EndpointAddr::new(key.public());
        assert!(matches!(
            relay_mode_for_addr(&addr).unwrap(),
            RelayMode::Disabled
        ));
        addr.addrs.insert(TransportAddr::Relay(
            "http://127.0.0.1:7479/".parse().unwrap(),
        ));
        match relay_mode_for_addr(&addr).unwrap() {
            RelayMode::Custom(map) => {
                let urls = map.urls::<Vec<_>>();
                assert_eq!(urls[0].as_str(), "http://127.0.0.1:7479/");
            }
            other => panic!("expected a custom relay mode, got {other:?}"),
        }
    }

    async fn mesh_echo_once(through_relay: bool) {
        let home_a = temp_home(if through_relay {
            "echo-relay-a"
        } else {
            "echo-a"
        });
        let home_b = temp_home(if through_relay {
            "echo-relay-b"
        } else {
            "echo-b"
        });
        let (relay_url, _relay) = if through_relay {
            let mut config = iroh_relay::server::ServerConfig::default();
            config.relay = Some(iroh_relay::server::RelayConfig::new(
                "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
            ));
            let relay = iroh_relay::server::Server::spawn(config).await.unwrap();
            let url = format!("http://{}/", relay.http_addr().unwrap());
            for home in [&home_a, &home_b] {
                std::fs::write(saved_relay_path(home), &url).unwrap();
            }
            (Some(url), Some(relay))
        } else {
            (None, None)
        };

        let key_a = load_node_key_at(&home_a).unwrap();
        let endpoint_a = Arc::new(build_node_endpoint_at(key_a, &home_a).await.unwrap());
        let key_b = load_node_key_at(&home_b).unwrap();
        let endpoint_b = Arc::new(build_node_endpoint_at(key_b, &home_b).await.unwrap());
        for endpoint in [&endpoint_a, &endpoint_b] {
            wait_for_relay(endpoint, relay_url.is_some()).await;
        }

        let stop = Arc::new(AtomicBool::new(false));
        let (mesh_tx, mut mesh_rx) = tokio_mpsc::unbounded_channel::<NodeIngressIo>();
        let _worker = spawn_mesh_accept_worker(endpoint_a.clone(), mesh_tx, stop.clone()).unwrap();

        let addr_a = endpoint_addr(&endpoint_a).await;
        let mut dial_addr = EndpointAddr::new(endpoint_a.id());
        for entry in addr_a.addrs {
            dial_addr.addrs.insert(entry);
        }
        if through_relay {
            dial_addr
                .addrs
                .insert(TransportAddr::Ip("127.0.0.1:9".parse().unwrap()));
        }
        let conn = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            endpoint_b.connect(dial_addr, MESH_SESSION_ALPN),
        )
        .await
        .expect("mesh dial timed out")
        .unwrap();
        let (send, recv) = conn.open_bi().await.unwrap();
        let mut client = MeshStream {
            send,
            recv,
            _endpoint: endpoint_b.clone(),
        };
        client.write_all(b"PING").await.unwrap();

        let ingress = tokio::time::timeout(std::time::Duration::from_secs(30), mesh_rx.recv())
            .await
            .expect("no mesh ingress arrived")
            .expect("ingress channel closed");
        let mut server = match ingress {
            NodeIngressIo::Mesh(connection) => connection,
            NodeIngressIo::Tcp(_) | NodeIngressIo::Relay(_) => {
                panic!("expected the mesh ingress arm")
            }
        };
        let mut buf = [0u8; 4];
        server.read_exact(&mut buf).await.unwrap();
        server.write_all(&buf).await.unwrap();
        let mut pong = [0u8; 4];
        client.read_exact(&mut pong).await.unwrap();
        assert_eq!(&pong, b"PING");

        drop(client);
        server.shutdown().await.unwrap();
        stop.store(true, Ordering::Relaxed);
        drop(endpoint_a);
        drop(endpoint_b);
        crate::infra::best_effort::remove_dir_all(&home_a);
        crate::infra::best_effort::remove_dir_all(&home_b);
    }

    /// Waits (briefly) for a direct address so the published addr is dialable.
    async fn endpoint_addr(endpoint: &Endpoint) -> EndpointAddr {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let addr = endpoint.addr();
            if addr
                .addrs
                .iter()
                .any(|entry| matches!(entry, TransportAddr::Ip(_)))
            {
                return addr;
            }
            if tokio::time::Instant::now() >= deadline {
                return addr;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    async fn wait_for_relay(endpoint: &Endpoint, expect_relay: bool) {
        for _ in 0..300 {
            let found = endpoint
                .addr()
                .addrs
                .iter()
                .any(|entry| matches!(entry, TransportAddr::Relay(_)));
            if found == expect_relay {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        panic!("relay presence never settled");
    }

    #[tokio::test]
    async fn loopback_mesh_session_over_the_direct_path() {
        mesh_echo_once(false).await;
    }

    #[tokio::test]
    async fn loopback_mesh_session_forced_through_the_relay() {
        mesh_echo_once(true).await;
    }
}
