use std::path::{Path, PathBuf};

use iroh::{Endpoint, EndpointAddr, RelayMode, SecretKey, TransportAddr};

use crate::cli::{CoordinationCommand, CoordinationSubcommand};
use crate::error::AppError;
use crate::infra::coordination::server;
use crate::infra::coordination::{ADMIN_ALPN, NODE_ALPN};
use crate::lifecycle::LifecycleError;

/// Entry point of the `coordination` CLI.
pub fn run(command: CoordinationCommand) -> Result<(), AppError> {
    match command.subcommand {
        CoordinationSubcommand::Serve {
            listen,
            admin_stdout,
        } => run_serve(listen, admin_stdout),
        CoordinationSubcommand::EnrollRequest { coordination } => run_enroll_request(&coordination),
        CoordinationSubcommand::Join { link, coordination } => {
            run_join(&link, coordination.as_deref())
        }
        CoordinationSubcommand::Requests { coordination } => {
            run_admin(&coordination, "{\"command\":\"requests\"}")
        }
        CoordinationSubcommand::Presence { coordination } => {
            run_admin(&coordination, "{\"command\":\"presence\"}")
        }
        CoordinationSubcommand::Heartbeat {
            coordination,
            every,
        } => run_heartbeat(coordination.as_deref(), every),
        CoordinationSubcommand::Resolve {
            node_id,
            coordination,
        } => run_resolve(&node_id, &coordination),
        CoordinationSubcommand::Approve {
            request_id,
            coordination,
        } => run_admin(
            &coordination,
            &format!("{{\"command\":\"approve\",\"request_id\":\"{request_id}\"}}"),
        ),
        CoordinationSubcommand::Revoke {
            node_id,
            coordination,
        } => run_admin(
            &coordination,
            &format!("{{\"command\":\"revoke\",\"node_id\":\"{node_id}\"}}"),
        ),
    }
}

fn run_serve(listen: Option<String>, admin_stdout: bool) -> Result<(), AppError> {
    let home = crate::infra::coordination::default_home();
    crate::infra::coordination::enrollment::create_private_dir(&home)
        .map_err(|error| io_error("create coordination home", error))?;
    let config_path = crate::infra::coordination::toml_store::default_config_path();
    let config = crate::infra::coordination::toml_store::CoordinationConfig::load(&config_path)
        .map_err(|error| protocol_error(error.to_string()))?
        .unwrap_or_default();
    let key_path = home.join("coordination-server.key");
    let key = load_key_notice(&key_path, "server")?;
    let listen = listen
        .map(|text| {
            text.parse::<std::net::SocketAddr>()
                .map_err(|error| protocol_error(format!("invalid --listen {text:?}: {error}")))
        })
        .transpose()?;
    let stores = server::CoordinationStores::open(&home)
        .map_err(|error| io_error("open coordination stores", error))?;
    let runtime = runtime()?;
    runtime
        .block_on(server::serve(
            key,
            listen,
            server::ServeParams {
                config,
                stores: std::sync::Arc::new(stores),
                admin_stdout,
                relay_url: None,
            },
        ))
        .map_err(protocol_error)
}

fn run_enroll_request(coordination: &str) -> Result<(), AppError> {
    let key_path = node_key_path();
    let key = load_key_notice(&key_path, "node")?;
    let node_addr = parse_coordination_addr(coordination).map_err(protocol_error)?;
    crate::infra::coordination::enrollment::write_private_file(
        &saved_coordination_path(),
        node_addr_string(&node_addr).as_bytes(),
    )
    .map_err(|error| io_error("save coordination address", error))?;
    let frame = serde_json::json!({
        "command": "enroll-request",
        "node_id": key.public().to_string(),
    });
    let response = runtime()?
        .block_on(round_trip(key, node_addr, NODE_ALPN, &frame))
        .map_err(protocol_error)?;
    print_response(&response)?;
    save_announced_relay(&response);
    Ok(())
}

fn run_join(link: &str, coordination: Option<&str>) -> Result<(), AppError> {
    let token = extract_join_token(link).map_err(protocol_error)?;
    let key_path = node_key_path();
    let key = load_key_notice(&key_path, "node")?;
    let node_addr = match coordination {
        Some(text) => parse_coordination_addr(text).map_err(protocol_error)?,
        None => {
            let saved = std::fs::read_to_string(saved_coordination_path())
                .map_err(|_| protocol_error("no --coordination given and no saved coordination address; run enroll-request first".to_string()))?;
            crate::infra::mesh_probe::parse_node_addr(&saved).map_err(protocol_error)?
        }
    };
    let frame = serde_json::json!({
        "command": "join",
        "node_id": key.public().to_string(),
        "token": token,
    });
    let response = runtime()?
        .block_on(round_trip(
            key.clone(),
            node_addr.clone(),
            NODE_ALPN,
            &frame,
        ))
        .map_err(protocol_error)?;
    print_response(&response)?;
    if response.get("ok") == Some(&serde_json::Value::Bool(true)) {
        save_announced_relay(&response);
        let relay = announced_or_saved_relay(&response);
        if let Err(message) = heartbeat_round_trip(&key, node_addr, relay.as_deref()) {
            eprintln!("post-join heartbeat failed: {message}");
        }
    }
    Ok(())
}

fn run_resolve(node_id: &str, coordination: &str) -> Result<(), AppError> {
    node_id
        .parse::<iroh::PublicKey>()
        .map_err(|error| protocol_error(format!("invalid node id {node_id:?}: {error}")))?;
    let key_path = node_key_path();
    let key = load_key_notice(&key_path, "node")?;
    let node_addr = parse_coordination_addr(coordination).map_err(protocol_error)?;
    let frame = serde_json::json!({
        "command": "resolve",
        "node_id": node_id,
    });
    let response = runtime()?
        .block_on(round_trip(key, node_addr, NODE_ALPN, &frame))
        .map_err(protocol_error)?;
    print_response(&response)?;
    Ok(())
}

fn run_heartbeat(coordination: Option<&str>, every: Option<u64>) -> Result<(), AppError> {
    let interval = every.unwrap_or(0);
    if every.is_some() && interval == 0 {
        return Err(protocol_error("--every must be greater than 0".to_string()));
    }
    let key_path = node_key_path();
    let key = load_key_notice(&key_path, "node")?;
    let node_addr = match coordination {
        Some(text) => parse_coordination_addr(text).map_err(protocol_error)?,
        None => {
            let saved = std::fs::read_to_string(saved_coordination_path())
                .map_err(|_| protocol_error("no --coordination given and no saved coordination address; run enroll-request first".to_string()))?;
            crate::infra::mesh_probe::parse_node_addr(&saved).map_err(protocol_error)?
        }
    };
    let relay = load_saved_relay();
    loop {
        let response = heartbeat_round_trip(&key, node_addr.clone(), relay.as_deref())
            .map_err(protocol_error)?;
        print_response(&response)?;
        if interval == 0 {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_secs(interval));
    }
}

fn heartbeat_round_trip(
    key: &iroh::SecretKey,
    node_addr: iroh::EndpointAddr,
    relay: Option<&str>,
) -> Result<serde_json::Value, String> {
    let mut addrs = Vec::new();
    if let Some(url) = relay {
        addrs
            .push(iroh::TransportAddr::Relay(url.parse().map_err(
                |error| format!("invalid relay url {url:?}: {error}"),
            )?));
    }
    let addr = iroh::EndpointAddr::from_parts(key.public(), addrs);
    let frame = serde_json::json!({
        "command": "heartbeat",
        "node_id": key.public().to_string(),
        "addr": node_addr_string(&addr),
    });
    round_trip_blocking(key.clone(), node_addr, NODE_ALPN, &frame)
}

fn save_announced_relay(response: &serde_json::Value) {
    let Some(url) = response
        .get("relay_url")
        .and_then(serde_json::Value::as_str)
    else {
        return;
    };
    if let Err(error) = crate::infra::coordination::enrollment::write_private_file(
        &saved_relay_path(),
        url.as_bytes(),
    ) {
        eprintln!("could not save the announced relay url: {error}");
    }
}

fn announced_or_saved_relay(response: &serde_json::Value) -> Option<String> {
    response
        .get("relay_url")
        .and_then(serde_json::Value::as_str)
        .map(|url| url.to_string())
        .or_else(load_saved_relay)
}

fn load_saved_relay() -> Option<String> {
    std::fs::read_to_string(saved_relay_path())
        .ok()
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
}

fn run_admin(coordination: &str, body: &str) -> Result<(), AppError> {
    let key_path = operator_key_path();
    let key = load_key_notice(&key_path, "operator")?;
    let node_addr = parse_coordination_addr(coordination).map_err(protocol_error)?;
    let (body, operator, signature) = server::sign_admin_body(&key, body);
    let frame = serde_json::json!({
        "body": body,
        "operator": operator,
        "signature": signature,
    });
    let response = runtime()?
        .block_on(round_trip(key, node_addr, ADMIN_ALPN, &frame))
        .map_err(protocol_error)?;
    print_response(&response)?;
    if response.get("ok") == Some(&serde_json::Value::Bool(true)) {
        if let Some(link) = response.get("link").and_then(serde_json::Value::as_str) {
            println!("join_link: {link}");
        }
    }
    Ok(())
}

/// Runs a single coordination request on a self-built runtime for sync callers.
///
/// Transport and JSON failures are returned as `Err`; a well-formed JSON
/// response (including `{"ok": false, ...}`) is returned as `Ok` so callers
/// can treat any reply as liveness.
pub fn round_trip_blocking(
    key: SecretKey,
    node_addr: EndpointAddr,
    alpn: &[u8],
    frame: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    runtime()
        .map_err(|error| format!("build coordination runtime: {error}"))?
        .block_on(round_trip(key, node_addr, alpn, frame))
}

/// Returns the server-reported error for an `{"ok": false, ...}` response.
pub fn response_error(response: &serde_json::Value) -> Option<String> {
    if response.get("ok") == Some(&serde_json::Value::Bool(false)) {
        return Some(
            response
                .get("error")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("coordination request failed")
                .to_string(),
        );
    }
    None
}

async fn round_trip(
    key: SecretKey,
    node_addr: EndpointAddr,
    alpn: &[u8],
    frame: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let endpoint = Endpoint::builder(iroh::endpoint::presets::Minimal)
        .relay_mode(RelayMode::Disabled)
        .secret_key(key)
        .alpns(vec![alpn.to_vec()])
        .bind()
        .await
        .map_err(|error| format!("bind client endpoint: {error}"))?;
    let conn = endpoint
        .connect(node_addr, alpn)
        .await
        .map_err(|error| format!("connect: {error}"))?;
    let (mut send, mut recv) = conn
        .open_bi()
        .await
        .map_err(|error| format!("open stream: {error}"))?;
    let mut bytes = serde_json::to_vec(frame).unwrap();
    bytes.push(b'\n');
    send.write_all(&bytes)
        .await
        .map_err(|error| format!("write frame: {error}"))?;
    let _ = send.finish();
    let reply = recv
        .read_to_end(1024 * 1024)
        .await
        .map_err(|error| format!("read response: {error}"))?;
    endpoint.close().await;
    serde_json::from_slice(&reply).map_err(|error| format!("invalid response: {error}"))
}

fn print_response(response: &serde_json::Value) -> Result<(), AppError> {
    println!("{}", serde_json::to_string(response).unwrap());
    if response.get("ok") == Some(&serde_json::Value::Bool(false)) {
        return Err(protocol_error(
            response
                .get("error")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("coordination request failed")
                .to_string(),
        ));
    }
    Ok(())
}

/// Parses `node_addr` JSON or `<node-id>@<host:port>`.
pub fn parse_coordination_addr(text: &str) -> Result<EndpointAddr, String> {
    if let Ok(addr) = crate::infra::mesh_probe::parse_node_addr(text) {
        return Ok(addr);
    }
    let Some((id, host)) = text.trim().split_once('@') else {
        return Err(format!(
            "invalid --coordination {text:?}: expected the node_addr JSON line or <node-id>@<host:port>"
        ));
    };
    let id = crate::infra::mesh_probe::parse_node_id(id)?;
    let ip = host
        .parse::<std::net::SocketAddr>()
        .map_err(|error| format!("invalid --coordination host {host:?}: {error}"))?;
    Ok(EndpointAddr::from_parts(id, [TransportAddr::Ip(ip)]))
}

/// Extracts the token from a join link or validates a bare token.
pub fn extract_join_token(link: &str) -> Result<String, String> {
    let trimmed = link.trim();
    let token = trimmed
        .split_once("token=")
        .map(|(_, rest)| rest.split(['&', ' ']).next().unwrap_or_default())
        .unwrap_or(trimmed);
    if token.len() == 64 && token.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return Ok(token.to_string());
    }
    Err("invalid join link: expected waitagent://join?token=<hex> or a bare token".to_string())
}

fn node_addr_string(addr: &EndpointAddr) -> String {
    crate::infra::mesh_probe::format_node_addr(addr)
}

pub fn node_key_path() -> PathBuf {
    crate::host::ssh::remote_host_home::waitagent_home().join("coordination-node.key")
}

pub fn operator_key_path() -> PathBuf {
    crate::infra::coordination::server::default_operator_key_path()
}

fn saved_coordination_path() -> PathBuf {
    crate::host::ssh::remote_host_home::waitagent_home().join("coordination-server.json")
}

fn saved_relay_path() -> PathBuf {
    crate::host::ssh::remote_host_home::waitagent_home().join("coordination-relay.json")
}

fn load_key_notice(path: &Path, role: &str) -> Result<SecretKey, AppError> {
    let existed = path.is_file();
    let key = crate::infra::coordination::enrollment::load_or_generate_key(path)
        .map_err(|error| io_error(format!("load {role} key {}", path.display()), error))?;
    if !existed {
        eprintln!(
            "created {role} key {} (pubkey {}); list the pubkey in coordination.toml operators to authorize admin actions",
            path.display(),
            key.public()
        );
    }
    Ok(key)
}

fn runtime() -> Result<tokio::runtime::Runtime, AppError> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| io_error("build coordination runtime", error))
}

fn io_error(context: impl Into<String>, error: io::Error) -> AppError {
    AppError::Lifecycle(LifecycleError::Io(context.into(), error))
}

fn protocol_error(message: String) -> AppError {
    AppError::Lifecycle(LifecycleError::Protocol(message))
}

use std::io;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_coordination_addr_accepts_json_and_compact_forms() {
        let key = SecretKey::generate();
        let mut addr = EndpointAddr::new(key.public());
        addr.addrs
            .insert(TransportAddr::Ip("127.0.0.1:7479".parse().unwrap()));
        let json = crate::infra::mesh_probe::format_node_addr(&addr);
        assert_eq!(parse_coordination_addr(&json).unwrap(), addr);

        let compact = format!("{}@127.0.0.1:7479", key.public());
        assert_eq!(parse_coordination_addr(&compact).unwrap(), addr);

        assert!(parse_coordination_addr("127.0.0.1:7479").is_err());
        assert!(parse_coordination_addr("bogus@127.0.0.1:7479").is_err());
        assert!(parse_coordination_addr("bogus@nope").is_err());
    }

    #[test]
    fn extract_join_token_accepts_url_and_bare_token() {
        let token = "ab".repeat(32);
        assert_eq!(extract_join_token(&token).unwrap(), token);
        assert_eq!(
            extract_join_token(&format!("waitagent://join?token={token}")).unwrap(),
            token
        );
        assert_eq!(
            extract_join_token(&format!("waitagent://join?token={token}&extra=1")).unwrap(),
            token
        );
        assert!(extract_join_token("waitagent://join").is_err());
        assert!(extract_join_token("short").is_err());
    }
}
