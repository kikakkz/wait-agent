//! Issue #224 slice 0 (spike): minimal iroh mesh data-plane probe.
//!
//! Two `waitagent mesh-probe` processes form an iroh QUIC connection
//! (direct UDP first, no relay in this slice) and measure RTT plus
//! echo throughput — the evaluation harness for replacing the self-built
//! mux relay data plane with iroh. Not production code: no TUI surface, no
//! help listing, fixed ALPN, JSON lines on stdout.
//!
//! Version note: pinned to iroh 0.92.0. Every iroh 1.x (and 0.93+)
//! release either declares rust-version 1.91 while this repo builds with
//! 1.88, or pulls `crypto_box 0.10.0-pre.0`, whose curve25519-dalek pin
//! collides with russh's. 0.92.0 (MSRV 1.85) is the newest resolvable
//! line; the toolchain bump to take 1.x lands with the slice-1 migration.
//!
//! Addressing note: the spike's `--addr` value is a self-describing JSON
//! line, `{"node_id":…,"relay_url":…,"direct_addresses":[…]}`, printed by
//! `mesh-probe listen` — iroh 0.92's `NodeAddr` has no string form of its
//! own. The node id is iroh's ed25519 public key (z-base-32), which is the
//! identity shape the migration maps the existing node-certificate
//! fingerprints onto (issue #224 定稿).

use std::fs;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use iroh::{Endpoint, NodeAddr, NodeId, PublicKey, RelayMode, SecretKey, Watcher};

/// ALPN identifying mesh-probe connections. Fixed for the spike; the
/// production data plane negotiates its own set in slice 2.
pub const MESH_PROBE_ALPN: &[u8] = b"waitagent/mesh-probe/0";

/// Echo chunk for the throughput phase.
const ECHO_CHUNK: usize = 1024 * 1024;

/// Default probe key: `~/.waitagent/mesh-probe.key`.
pub fn default_key_path() -> PathBuf {
    crate::host::ssh::remote_host_home::waitagent_home().join("mesh-probe.key")
}

// --- pure logic (unit-tested below) ---

/// Parses an iroh node id (z-base-32 ed25519 public key).
pub fn parse_node_id(text: &str) -> Result<NodeId, String> {
    text.trim()
        .parse::<PublicKey>()
        .map_err(|error| format!("invalid node id {text:?}: {error}"))
}

/// Serializes a [`NodeAddr`] as one self-describing JSON line (the spike's
/// `--addr` wire format).
pub fn format_node_addr(addr: &NodeAddr) -> String {
    serde_json::json!({
        "node_id": addr.node_id.to_string(),
        "relay_url": addr.relay_url.as_ref().map(|url| url.to_string()),
        "direct_addresses": addr
            .direct_addresses
            .iter()
            .map(SocketAddr::to_string)
            .collect::<Vec<_>>(),
    })
    .to_string()
}

/// Parses the JSON line produced by [`format_node_addr`].
pub fn parse_node_addr(text: &str) -> Result<NodeAddr, String> {
    let value: serde_json::Value =
        serde_json::from_str(text.trim()).map_err(|error| format!("invalid node addr: {error}"))?;
    let node_id = value
        .get("node_id")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "node addr lacks `node_id`".to_string())?;
    let relay_url = value
        .get("relay_url")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let mut direct_addresses = std::collections::BTreeSet::new();
    if let Some(addrs) = value
        .get("direct_addresses")
        .and_then(serde_json::Value::as_array)
    {
        for entry in addrs {
            let text = entry
                .as_str()
                .ok_or_else(|| "direct_addresses entries must be strings".to_string())?;
            direct_addresses.insert(
                text.parse::<SocketAddr>()
                    .map_err(|error| format!("invalid direct address {text:?}: {error}"))?,
            );
        }
    }
    let mut addr = NodeAddr::new(parse_node_id(node_id)?);
    addr.direct_addresses = direct_addresses;
    if let Some(url) = relay_url {
        addr.relay_url = Some(
            url.parse()
                .map_err(|error| format!("invalid relay url {url:?}: {error}"))?,
        );
    }
    Ok(addr)
}

/// Loads the probe's persistent ed25519 key, generating and storing it on
/// first use. The key IS the node identity: same file → same node id.
pub fn load_or_generate_secret_key(path: &Path) -> io::Result<SecretKey> {
    if let Ok(bytes) = fs::read(path) {
        let bytes: [u8; 32] = bytes.as_slice().try_into().map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "mesh probe key is not 32 bytes")
        })?;
        return Ok(SecretKey::from_bytes(&bytes));
    }
    let key = SecretKey::generate(rand::rngs::OsRng);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    write_private_file(path, &key.to_bytes())?;
    Ok(key)
}

/// Writes a private key file 0o600, mirroring `node_credentials`' key-file
/// permission convention (the node credentials helper is PEM-specific).
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

// --- the probe itself ---

/// `mesh-probe listen`: prints this node's id and address line, then
/// echoes every inbound bidi stream until interrupted.
pub async fn listen(key_path: &Path) -> Result<(), String> {
    let key = load_or_generate_secret_key(key_path)
        .map_err(|error| format!("load probe key {}: {error}", key_path.display()))?;
    let endpoint = Endpoint::builder()
        .relay_mode(RelayMode::Disabled)
        .secret_key(key)
        .alpns(vec![MESH_PROBE_ALPN.to_vec()])
        .bind()
        .await
        .map_err(|error| format!("bind mesh endpoint: {error}"))?;
    // Wait briefly for magicsock to enumerate interfaces so the printed
    // address is actually dialable (with relay disabled the watcher only
    // needs direct addresses).
    let node_addr = {
        let mut watcher = endpoint.node_addr();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(addr) = watcher.get() {
                if !addr.direct_addresses.is_empty() {
                    break addr;
                }
            }
            if Instant::now() >= deadline {
                break NodeAddr::new(endpoint.node_id());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    };
    println!("node_id: {}", endpoint.node_id());
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

/// Echoes one accepted connection: every received chunk is written back
/// until the peer half-closes.
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

/// `mesh-probe dial`: connects, measures the handshake RTT with a 4-byte
/// ping-pong, then echoes `seconds` worth of 1 MiB chunks and prints one
/// JSON result line.
pub async fn dial(
    key_path: &Path,
    node_id: NodeId,
    addr: NodeAddr,
    seconds: u64,
) -> Result<(), String> {
    if addr.node_id != node_id {
        return Err(format!(
            "--node-id {} does not match the address's node id {}",
            node_id, addr.node_id
        ));
    }
    let key = load_or_generate_secret_key(key_path)
        .map_err(|error| format!("load probe key {}: {error}", key_path.display()))?;
    let endpoint = Endpoint::builder()
        .relay_mode(RelayMode::Disabled)
        .secret_key(key)
        .alpns(vec![MESH_PROBE_ALPN.to_vec()])
        .bind()
        .await
        .map_err(|error| format!("bind mesh endpoint: {error}"))?;

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

    #[test]
    fn node_addr_round_trips_through_the_wire_format() {
        let mut addr = NodeAddr::new(parse_node_id(&node_id_string()).unwrap());
        addr.direct_addresses
            .insert("192.168.1.5:7777".parse().unwrap());
        addr.direct_addresses
            .insert("127.0.0.1:7777".parse().unwrap());
        let text = format_node_addr(&addr);
        let parsed = parse_node_addr(&text).unwrap();
        assert_eq!(parsed, addr);
        assert_eq!(parsed.node_id, addr.node_id);
        assert_eq!(parsed.direct_addresses.len(), 2, "both addrs survive");
        assert!(parsed.relay_url.is_none());
    }

    #[test]
    fn node_addr_wire_format_preserves_a_relay_url() {
        let mut addr = NodeAddr::new(parse_node_id(&node_id_string()).unwrap());
        addr.relay_url = Some("https://relay.example:443".parse().unwrap());
        let parsed = parse_node_addr(&format_node_addr(&addr)).unwrap();
        assert_eq!(parsed.relay_url, addr.relay_url);
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
        assert_eq!(
            first.public(),
            second.public(),
            "reloading the key file reproduces the node id"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "the key file follows the 0o600 convention");
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

    /// Any valid node id string (a freshly generated key's).
    fn node_id_string() -> String {
        SecretKey::generate(rand::rngs::OsRng).public().to_string()
    }
}
