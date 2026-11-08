//! Mesh data-plane probe for the iroh evaluation (issue #224; see docs/relay-design.md).

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::net::{SocketAddr, SocketAddrV4};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use iroh::endpoint::presets;
use iroh::{Endpoint, EndpointAddr, EndpointId, PublicKey, RelayMode, SecretKey, TransportAddr};

pub const MESH_PROBE_ALPN: &[u8] = b"waitagent/mesh-probe/0";

const ECHO_CHUNK: usize = 1024 * 1024;

pub fn default_key_path() -> PathBuf {
    crate::host::ssh::remote_host_home::waitagent_home().join("mesh-probe.key")
}

pub fn parse_bind_addr(text: &str) -> Result<SocketAddrV4, String> {
    text.trim()
        .parse::<SocketAddrV4>()
        .map_err(|error| format!("invalid bind address {text:?}: {error}"))
}

pub fn parse_node_id(text: &str) -> Result<EndpointId, String> {
    text.trim()
        .parse::<PublicKey>()
        .map_err(|error| format!("invalid node id {text:?}: {error}"))
}

pub fn format_node_addr(addr: &EndpointAddr) -> String {
    serde_json::json!({
        "node_id": addr.id.to_string(),
        "relay_url": addr
            .addrs
            .iter()
            .find_map(|addr| match addr {
                TransportAddr::Relay(url) => Some(url.to_string()),
                _ => None,
            }),
        "direct_addresses": addr
            .addrs
            .iter()
            .filter_map(|addr| match addr {
                TransportAddr::Ip(ip) => Some(ip.to_string()),
                _ => None,
            })
            .collect::<Vec<_>>(),
    })
    .to_string()
}

pub fn parse_node_addr(text: &str) -> Result<EndpointAddr, String> {
    let value: serde_json::Value =
        serde_json::from_str(text.trim()).map_err(|error| format!("invalid node addr: {error}"))?;
    let node_id = value
        .get("node_id")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "node addr lacks `node_id`".to_string())?;
    let mut addrs = BTreeSet::new();
    if let Some(url) = value.get("relay_url").and_then(serde_json::Value::as_str) {
        addrs
            .insert(TransportAddr::Relay(url.parse().map_err(|error| {
                format!("invalid relay url {url:?}: {error}")
            })?));
    }
    if let Some(list) = value
        .get("direct_addresses")
        .and_then(serde_json::Value::as_array)
    {
        for entry in list {
            let text = entry
                .as_str()
                .ok_or_else(|| "direct_addresses entries must be strings".to_string())?;
            addrs.insert(TransportAddr::Ip(text.parse::<SocketAddr>().map_err(
                |error| format!("invalid direct address {text:?}: {error}"),
            )?));
        }
    }
    Ok(EndpointAddr::from_parts(parse_node_id(node_id)?, addrs))
}

pub fn load_or_generate_secret_key(path: &Path) -> io::Result<SecretKey> {
    if let Ok(bytes) = fs::read(path) {
        let bytes: [u8; 32] = bytes.as_slice().try_into().map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "mesh probe key is not 32 bytes")
        })?;
        return Ok(SecretKey::from_bytes(&bytes));
    }
    let key = SecretKey::generate();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    write_private_file(path, &key.to_bytes())?;
    Ok(key)
}

fn write_private_file(path: &Path, contents: &[u8]) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(contents)?;
        file.flush()?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        fs::write(path, contents)
    }
}

async fn build_endpoint(
    key: SecretKey,
    bind: Option<SocketAddrV4>,
    relay: Option<&str>,
) -> Result<Endpoint, String> {
    let relay_mode = match relay {
        Some(url) => {
            let map = iroh::RelayMap::try_from_iter([url])
                .map_err(|error| format!("invalid relay url {url:?}: {error}"))?;
            RelayMode::Custom(map)
        }
        None => RelayMode::Disabled,
    };
    let mut builder = Endpoint::builder(presets::Minimal).relay_mode(relay_mode);
    if let Some(addr) = bind {
        builder = builder
            .bind_addr(SocketAddr::V4(addr))
            .map_err(|error| format!("invalid bind address {addr}: {error}"))?;
    }
    builder
        .secret_key(key)
        .alpns(vec![MESH_PROBE_ALPN.to_vec()])
        .bind()
        .await
        .map_err(|error| format!("bind mesh endpoint: {error}"))
}

pub async fn listen(
    key_path: &Path,
    bind: Option<SocketAddrV4>,
    relay: Option<&str>,
) -> Result<(), String> {
    let key = load_or_generate_secret_key(key_path)
        .map_err(|error| format!("load probe key {}: {error}", key_path.display()))?;
    let endpoint = build_endpoint(key, bind, relay).await?;
    let deadline = Instant::now() + Duration::from_secs(5);
    let node_addr = loop {
        let addr = endpoint.addr();
        let has_ip = addr
            .addrs
            .iter()
            .any(|entry| matches!(entry, TransportAddr::Ip(_)));
        let has_relay = relay.is_none()
            || addr
                .addrs
                .iter()
                .any(|entry| matches!(entry, TransportAddr::Relay(_)));
        if has_ip && has_relay {
            break addr;
        }
        if Instant::now() >= deadline {
            break iroh::EndpointAddr::new(endpoint.id());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    println!("node_id: {}", endpoint.id());
    println!("node_addr: {}", format_node_addr(&node_addr));
    println!("mesh-probe listening; ctrl-c to stop");
    loop {
        let Some(incoming) = endpoint.accept().await else {
            return Err("mesh endpoint closed".to_string());
        };
        let Ok(connecting) = incoming.accept() else {
            continue;
        };
        tokio::spawn(async move {
            if let Ok(conn) = connecting.await {
                if let Err(error) = echo_session(conn).await {
                    eprintln!("mesh-probe session ended: {error}");
                }
            }
        });
    }
}

async fn echo_session(conn: iroh::endpoint::Connection) -> Result<(), String> {
    let (mut send, mut recv) = conn
        .accept_bi()
        .await
        .map_err(|error| format!("accept bi stream: {error}"))?;
    let mut buf = vec![0u8; ECHO_CHUNK];
    loop {
        let read = recv
            .read(&mut buf)
            .await
            .map_err(|error| format!("echo read: {error}"))?;
        let Some(len) = read else {
            break;
        };
        send.write_all(&buf[..len])
            .await
            .map_err(|error| format!("echo write: {error}"))?;
    }
    let _ = send.finish();
    Ok(())
}

pub async fn dial(
    key_path: &Path,
    node_id: EndpointId,
    addr: EndpointAddr,
    seconds: u64,
    relay: Option<&str>,
) -> Result<(), String> {
    if addr.id != node_id {
        return Err(format!(
            "--node-id {} does not match the address's node id {}",
            node_id, addr.id
        ));
    }
    let key = load_or_generate_secret_key(key_path)
        .map_err(|error| format!("load probe key {}: {error}", key_path.display()))?;
    let endpoint = build_endpoint(key, None, relay).await?;

    let started = Instant::now();
    let conn = endpoint
        .connect(addr, MESH_PROBE_ALPN)
        .await
        .map_err(|error| format!("connect: {error}"))?;
    let connect_ms = started.elapsed().as_secs_f64() * 1000.0;

    let (mut send, mut recv) = conn
        .open_bi()
        .await
        .map_err(|error| format!("open bi stream: {error}"))?;

    let ping_started = Instant::now();
    send.write_all(b"PING")
        .await
        .map_err(|error| format!("ping write: {error}"))?;
    let mut pong = [0u8; 4];
    recv.read_exact(&mut pong)
        .await
        .map_err(|error| format!("ping read: {error}"))?;
    let rtt_ms = ping_started.elapsed().as_secs_f64() * 1000.0;

    let chunk = vec![0xABu8; ECHO_CHUNK];
    let mut echoed = vec![0u8; ECHO_CHUNK];
    let mut total_bytes = 0u64;
    let window = Instant::now();
    while window.elapsed() < Duration::from_secs(seconds) {
        send.write_all(&chunk)
            .await
            .map_err(|error| format!("throughput write: {error}"))?;
        recv.read_exact(&mut echoed)
            .await
            .map_err(|error| format!("throughput read: {error}"))?;
        total_bytes += chunk.len() as u64;
    }
    let elapsed = window.elapsed().as_secs_f64();
    let echo_mib_per_sec = (total_bytes as f64 / elapsed) / (1024.0 * 1024.0);
    println!(
        "{}",
        serde_json::json!({
            "ok": true,
            "node_id": node_id.to_string(),
            "relay": relay,
            "connect_ms": (connect_ms * 100.0).round() / 100.0,
            "rtt_ms": (rtt_ms * 100.0).round() / 100.0,
            "echo_mib_per_sec": (echo_mib_per_sec * 100.0).round() / 100.0,
            "seconds": (elapsed * 100.0).round() / 100.0,
        })
    );
    endpoint.close().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_key_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "waitagent-mesh-probe-{name}-{}-{}.key",
            std::process::id(),
            std::thread::current()
                .name()
                .unwrap_or("test")
                .replace(':', "_")
        ))
    }

    fn addr_with_relay() -> EndpointAddr {
        let mut addr = EndpointAddr::new(parse_node_id(&node_id_string()).unwrap());
        addr.addrs.insert(TransportAddr::Relay(
            "https://relay.example:443".parse().unwrap(),
        ));
        addr.addrs
            .insert(TransportAddr::Ip("192.168.1.5:7777".parse().unwrap()));
        addr.addrs
            .insert(TransportAddr::Ip("127.0.0.1:7777".parse().unwrap()));
        addr
    }

    #[test]
    fn node_addr_round_trips_through_the_wire_format() {
        let addr = addr_with_relay();
        let parsed = parse_node_addr(&format_node_addr(&addr)).unwrap();
        assert_eq!(parsed, addr);
        assert_eq!(parsed.id, addr.id);
        assert_eq!(
            parsed
                .addrs
                .iter()
                .filter(|entry| matches!(entry, TransportAddr::Ip(_)))
                .count(),
            2,
            "both direct addrs survive"
        );
        assert!(parsed
            .addrs
            .iter()
            .any(|entry| matches!(entry, TransportAddr::Relay(_))));
    }

    #[test]
    fn parse_bind_addr_accepts_v4_socket_addresses() {
        let addr = parse_bind_addr("0.0.0.0:7479").unwrap();
        assert_eq!(addr.to_string(), "0.0.0.0:7479");
        assert_eq!(
            parse_bind_addr(" 127.0.0.1:9999 \n").unwrap().to_string(),
            "127.0.0.1:9999"
        );
        assert!(parse_bind_addr(":::1").is_err());
    }

    #[test]
    fn parse_node_addr_rejects_garbage() {
        assert!(parse_node_addr("not json").is_err());
        assert!(parse_node_addr("{}").is_err(), "lacks node_id");
        assert!(parse_node_addr(
            &serde_json::json!({"node_id": "bogus", "direct_addresses": []}).to_string()
        )
        .is_err());
        assert!(parse_node_addr(
            &serde_json::json!({"node_id": node_id_string(), "direct_addresses": ["nope"]})
                .to_string()
        )
        .is_err());
    }

    #[test]
    fn parse_node_id_accepts_only_z_base_32_keys() {
        assert!(parse_node_id(&node_id_string()).is_ok());
        assert!(parse_node_id("").is_err());
        assert!(parse_node_id("deadbeef").is_err());
    }

    #[test]
    fn secret_key_persists_and_reloads_the_same_identity() {
        let path = temp_key_path("identity");
        let _ = fs::remove_file(&path);
        let first = load_or_generate_secret_key(&path).unwrap();
        let second = load_or_generate_secret_key(&path).unwrap();
        assert_eq!(first.public(), second.public());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        crate::infra::best_effort::remove_file(&path);
    }

    #[test]
    fn secret_key_rejects_a_truncated_file() {
        let path = temp_key_path("truncated");
        fs::write(&path, b"short").unwrap();
        assert!(load_or_generate_secret_key(&path).is_err());
        crate::infra::best_effort::remove_file(&path);
    }

    fn node_id_string() -> String {
        SecretKey::generate().public().to_string()
    }

    #[tokio::test]
    async fn relay_mode_connects_through_the_relay_with_no_direct_path() {
        let mut config = iroh_relay::server::ServerConfig::default();
        config.relay = Some(iroh_relay::server::RelayConfig::new(
            "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        ));
        let relay = iroh_relay::server::Server::spawn(config).await.unwrap();
        let url = format!("http://{}/", relay.http_addr().unwrap());

        let listener = build_endpoint(SecretKey::generate(), None, Some(&url))
            .await
            .unwrap();
        let mut listener_addr = EndpointAddr::new(listener.id());
        for _ in 0..100 {
            let addr = listener.addr();
            if addr
                .addrs
                .iter()
                .any(|entry| matches!(entry, TransportAddr::Relay(_)))
            {
                listener_addr = addr;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            listener_addr
                .addrs
                .iter()
                .any(|entry| matches!(entry, TransportAddr::Relay(found) if found.as_str() == url)),
            "the listener publishes its relay url"
        );

        let mut dial_addr = EndpointAddr::new(listener.id());
        dial_addr
            .addrs
            .insert(TransportAddr::Relay(url.parse().expect("relay url parses")));
        dial_addr
            .addrs
            .insert(TransportAddr::Ip("127.0.0.1:9".parse().unwrap()));

        let dialer = build_endpoint(SecretKey::generate(), None, Some(&url))
            .await
            .unwrap();
        for _ in 0..100 {
            if dialer
                .addr()
                .addrs
                .iter()
                .any(|entry| matches!(entry, TransportAddr::Relay(_)))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        let accept = tokio::spawn({
            let listener = listener.clone();
            async move {
                let incoming = listener.accept().await.unwrap();
                let connecting = incoming.accept().unwrap();
                assert!(
                    matches!(
                        connecting.remote_addr(),
                        iroh::endpoint::IncomingAddr::Relay { .. }
                    ),
                    "the listener observes a relayed connection: {:?}",
                    connecting.remote_addr()
                );
                connecting.await.unwrap()
            }
        });
        let connect = tokio::time::timeout(
            Duration::from_secs(30),
            dialer.connect(dial_addr, MESH_PROBE_ALPN),
        );
        let conn = connect.await.expect("relay dial timed out").unwrap();

        let server_conn = accept.await.unwrap();
        let echo = tokio::spawn(echo_session(server_conn));
        let (mut send, mut recv) = conn.open_bi().await.unwrap();
        send.write_all(b"PING").await.unwrap();
        let mut pong = [0u8; 4];
        recv.read_exact(&mut pong).await.unwrap();
        assert_eq!(&pong, b"PING");
        drop(send);
        drop(recv);
        echo.await.unwrap().unwrap();
        dialer.close().await;
    }

    #[tokio::test]
    async fn build_endpoint_rejects_an_invalid_relay_url() {
        let error = build_endpoint(SecretKey::generate(), None, Some("not a url"))
            .await
            .expect_err("an invalid relay url is refused");
        assert!(error.contains("invalid relay url"));
    }
}
