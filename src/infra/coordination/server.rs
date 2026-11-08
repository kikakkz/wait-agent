use std::path::Path;
use std::sync::Arc;

use iroh::{Endpoint, RelayMode, SecretKey};

use crate::infra::coordination::enrollment::{
    EnrollmentError, JoinRequest, RegistryStore, RequestState, RequestStore,
};
use crate::infra::coordination::presence::PresenceStore;
use crate::infra::coordination::toml_store::{self, CoordinationConfig};
use crate::infra::coordination::{ADMIN_ALPN, NODE_ALPN};

/// The request and registry stores behind one handle.
pub struct CoordinationStores {
    pub requests: RequestStore,
    pub registry: RegistryStore,
    pub presence: PresenceStore,
}

impl CoordinationStores {
    /// Opens both stores under `root`.
    pub fn open(root: &Path) -> std::io::Result<Self> {
        Ok(Self {
            requests: RequestStore::new(root)?,
            registry: RegistryStore::new(root)?,
            presence: PresenceStore::new(),
        })
    }
}

/// Runtime inputs of the coordination serve loop.
pub struct ServeParams {
    pub config: CoordinationConfig,
    pub stores: CoordinationStores,
    pub admin_stdout: bool,
    /// Public URL of the embedded relay, announced to joining nodes.
    pub relay_url: Option<String>,
}

/// Runs the coordination endpoint until the process stops.
pub async fn serve(
    key: SecretKey,
    listen: Option<std::net::SocketAddr>,
    params: ServeParams,
) -> Result<(), String> {
    let default_listen = std::net::SocketAddr::from((
        std::net::Ipv4Addr::UNSPECIFIED,
        crate::infra::coordination::DEFAULT_LISTEN_PORT,
    ));
    let mut builder =
        Endpoint::builder(iroh::endpoint::presets::Minimal).relay_mode(RelayMode::Disabled);
    if let Some(addr) = listen.or(params.config.listen).or(Some(default_listen)) {
        builder = builder
            .bind_addr(addr)
            .map_err(|error| format!("invalid listen address {addr}: {error}"))?;
    }
    let endpoint = builder
        .secret_key(key)
        .alpns(vec![NODE_ALPN.to_vec(), ADMIN_ALPN.to_vec()])
        .bind()
        .await
        .map_err(|error| format!("bind coordination endpoint: {error}"))?;
    let (_relay, relay_http_addr) = start_embedded_relay(&params.config, &endpoint).await;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let node_addr = loop {
        let addr = endpoint.addr();
        if addr
            .addrs
            .iter()
            .any(|entry| matches!(entry, iroh::TransportAddr::Ip(_)))
        {
            break addr;
        }
        if std::time::Instant::now() >= deadline {
            break iroh::EndpointAddr::new(endpoint.id());
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    };
    let relay_url = params.config.relay_url.clone().or_else(|| {
        relay_http_addr.and_then(|http_addr| {
            node_addr.addrs.iter().find_map(|entry| match entry {
                iroh::TransportAddr::Ip(ip) => {
                    Some(format!("http://{}:{}/", ip.ip(), http_addr.port()))
                }
                _ => None,
            })
        })
    });
    println!("node_id: {}", endpoint.id());
    println!(
        "node_addr: {}",
        crate::infra::mesh_probe::format_node_addr(&node_addr)
    );
    if let Some(url) = &relay_url {
        println!("relay_url: {url}");
    }
    println!("coordination listening; ctrl-c to stop");
    let params = Arc::new(ServeParams {
        relay_url,
        ..params
    });
    loop {
        let Some(incoming) = endpoint.accept().await else {
            return Err("coordination endpoint closed".to_string());
        };
        let params = params.clone();
        tokio::spawn(async move {
            let Ok(mut accepting) = incoming.accept() else {
                return;
            };
            let alpn = accepting.alpn().await.unwrap_or_default();
            let result = match alpn.as_slice() {
                NODE_ALPN => serve_node_stream(accepting, params).await,
                ADMIN_ALPN => serve_admin_stream(accepting, params).await,
                _ => Ok(()),
            };
            if let Err(error) = result {
                eprintln!("coordination connection ended: {error}");
            }
        });
    }
}

/// Starts the embedded iroh relay on the same port number as the QUIC endpoint.
///
/// A bind or spawn failure is logged and downgraded to "no relay": coordination
/// stays usable and the relay URL is simply not announced.
async fn start_embedded_relay(
    config: &CoordinationConfig,
    endpoint: &Endpoint,
) -> (
    Option<iroh_relay::server::Server>,
    Option<std::net::SocketAddr>,
) {
    if !config.relay_enabled() {
        return (None, None);
    }
    let Some(bound) = endpoint.bound_sockets().first().copied() else {
        return (None, None);
    };
    let bind = std::net::SocketAddr::new(bound.ip(), bound.port());
    let mut server_config = iroh_relay::server::ServerConfig::default();
    server_config.relay = Some(iroh_relay::server::RelayConfig::new(bind));
    let spawned = iroh_relay::server::Server::spawn(server_config).await;
    match spawned {
        Ok(server) => {
            let addr = server.http_addr();
            (Some(server), addr)
        }
        Err(error) => {
            eprintln!(
                "embedded relay failed to start on {bind}: {error}; continuing without relay"
            );
            (None, None)
        }
    }
}

async fn read_frame(recv: &mut iroh::endpoint::RecvStream) -> Result<serde_json::Value, String> {
    let bytes = recv
        .read_to_end(1024 * 1024)
        .await
        .map_err(|error| format!("read frame: {error}"))?;
    serde_json::from_slice(&bytes).map_err(|error| format!("invalid frame: {error}"))
}

async fn write_frame(
    send: &mut iroh::endpoint::SendStream,
    value: &serde_json::Value,
) -> Result<(), String> {
    let mut bytes = serde_json::to_vec(value).unwrap();
    bytes.push(b'\n');
    send.write_all(&bytes)
        .await
        .map_err(|error| format!("write frame: {error}"))?;
    let _ = send.finish();
    Ok(())
}

async fn serve_node_stream(
    connecting: iroh::endpoint::Accepting,
    params: Arc<ServeParams>,
) -> Result<(), String> {
    let observed = match connecting.remote_addr() {
        iroh::endpoint::IncomingAddr::Ip(addr) => Some(addr),
        _ => None,
    };
    let conn = connecting.await.map_err(|error| error.to_string())?;
    let requester = conn.remote_id().to_string();
    let (mut send, mut recv) = conn.accept_bi().await.map_err(|error| error.to_string())?;
    let frame = read_frame(&mut recv).await?;
    let response = handle_node_frame(&params, &frame, &requester, observed).await;
    write_frame(&mut send, &response).await?;
    let _ = conn.closed().await;
    Ok(())
}

async fn serve_admin_stream(
    connecting: iroh::endpoint::Accepting,
    params: Arc<ServeParams>,
) -> Result<(), String> {
    let conn = connecting.await.map_err(|error| error.to_string())?;
    let (mut send, mut recv) = conn.accept_bi().await.map_err(|error| error.to_string())?;
    let frame = read_frame(&mut recv).await?;
    let response = handle_admin_frame(&params, &frame);
    write_frame(&mut send, &response).await?;
    let _ = conn.closed().await;
    Ok(())
}

/// Executes one node-ALPN frame and returns the response.
///
/// `requester` is the connection-authenticated node id; `observed` is the
/// direct address the connection arrived from.
pub async fn handle_node_frame(
    params: &ServeParams,
    frame: &serde_json::Value,
    requester: &str,
    observed: Option<std::net::SocketAddr>,
) -> serde_json::Value {
    let command = frame.get("command").and_then(serde_json::Value::as_str);
    match command {
        Some("enroll-request") => {
            let node_id = frame
                .get("node_id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            if node_id.trim().is_empty() {
                return error_response("enroll-request requires a node_id");
            }
            if let Err(message) = node_id.parse::<iroh::PublicKey>() {
                return error_response(format!("invalid node_id: {message}"));
            }
            let now = now_unix();
            match params
                .stores
                .requests
                .create(node_id, params.config.token_ttl(), now)
            {
                Ok(request) => {
                    let notify = if params.admin_stdout {
                        println!("magic_link: {}", toml_store::magic_link(&request));
                        Ok(())
                    } else {
                        toml_store::send_enroll_magic_link(&params.config, &request).await
                    };
                    match notify {
                        Ok(()) => with_relay_url(
                            params,
                            serde_json::json!({
                                "ok": true,
                                "request_id": request.id,
                                "expires_at": request.expires_at,
                            }),
                        ),
                        Err(message) => {
                            let _ = params.stores.requests.save(&JoinRequest {
                                state: RequestState::Revoked,
                                ..request.clone()
                            });
                            error_response(message)
                        }
                    }
                }
                Err(error) => error_response(format!("store request: {error}")),
            }
        }
        Some("join") => {
            let token = frame
                .get("token")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            let node_id = frame
                .get("node_id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            match params.stores.requests.redeem(token, node_id, now_unix()) {
                Ok(request) => match params.stores.registry.register(node_id, now_unix()) {
                    Ok(()) => with_relay_url(
                        params,
                        serde_json::json!({"ok": true, "node_id": node_id, "request_id": request.id}),
                    ),
                    Err(error) => error_response(format!("register node: {error}")),
                },
                Err(error) => error_response(enrollment_message(&error)),
            }
        }
        Some("heartbeat") => {
            let node_id = frame
                .get("node_id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            if node_id != requester {
                return error_response("heartbeat node_id does not match the connection identity");
            }
            if !params.stores.registry.is_authorized(node_id) {
                return error_response("node is not registered");
            }
            let addr_text = frame
                .get("addr")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            let addr = if addr_text.trim().is_empty() {
                iroh::EndpointAddr::new(node_id.parse::<iroh::PublicKey>().unwrap())
            } else {
                match crate::infra::mesh_probe::parse_node_addr(addr_text) {
                    Ok(addr) if addr.id.to_string() == node_id => addr,
                    Ok(_) => return error_response("heartbeat addr belongs to another node"),
                    Err(message) => return error_response(message),
                }
            };
            params
                .stores
                .presence
                .record(node_id.to_string(), addr, observed, now_unix());
            with_relay_url(params, serde_json::json!({"ok": true}))
        }
        Some("resolve") => {
            let target = frame
                .get("node_id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            if let Err(message) = target.parse::<iroh::PublicKey>() {
                return error_response(format!("invalid node_id: {message}"));
            }
            if !params.stores.registry.is_authorized(requester) {
                return error_response("requester is not registered");
            }
            if !params.stores.registry.is_authorized(target) {
                return error_response(format!("node {target} is not registered"));
            }
            match params.stores.presence.get(target) {
                Some(entry) => serde_json::json!({
                    "ok": true,
                    "node_id": entry.node_id,
                    "node_addr": crate::infra::mesh_probe::format_node_addr(&entry.addr),
                    "last_seen": entry.last_seen,
                }),
                None => error_response(format!("no address for node {target}")),
            }
        }
        _ => error_response("unknown command"),
    }
}

fn with_relay_url(params: &ServeParams, mut response: serde_json::Value) -> serde_json::Value {
    if let Some(url) = &params.relay_url {
        response["relay_url"] = serde_json::Value::String(url.clone());
    }
    response
}

/// Verifies the operator signature and executes one admin-ALPN frame.
pub fn handle_admin_frame(params: &ServeParams, frame: &serde_json::Value) -> serde_json::Value {
    let body = frame.get("body").and_then(serde_json::Value::as_str);
    let operator = frame.get("operator").and_then(serde_json::Value::as_str);
    let signature = frame.get("signature").and_then(serde_json::Value::as_str);
    let (Some(body), Some(operator), Some(signature)) = (body, operator, signature) else {
        return error_response("admin frame requires body, operator, signature");
    };
    if !params
        .config
        .operators
        .iter()
        .any(|listed| listed == operator)
    {
        return error_response("operator is not authorized");
    }
    let Ok(public_key) = operator.parse::<iroh::PublicKey>() else {
        return error_response("operator is not a valid public key");
    };
    let Ok(signature_bytes) = hex_to_bytes(signature) else {
        return error_response("signature is not hex");
    };
    let Ok(signature_array) = <[u8; 64]>::try_from(signature_bytes.as_slice()) else {
        return error_response("signature must be 64 bytes");
    };
    let signature = iroh::Signature::from_bytes(&signature_array);
    if public_key.verify(body.as_bytes(), &signature).is_err() {
        return error_response("signature verification failed");
    }
    let Ok(body_value) = serde_json::from_str::<serde_json::Value>(body) else {
        return error_response("body is not json");
    };
    match body_value
        .get("command")
        .and_then(serde_json::Value::as_str)
    {
        Some("requests") => match params.stores.requests.list() {
            Ok(requests) => serde_json::json!({
                "ok": true,
                "requests": requests.iter().map(request_json).collect::<Vec<_>>(),
            }),
            Err(error) => error_response(format!("list requests: {error}")),
        },
        Some("approve") => {
            let request_id = body_value
                .get("request_id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            match params.stores.requests.approve(request_id, now_unix()) {
                Ok(request) => serde_json::json!({
                    "ok": true,
                    "request_id": request.id,
                    "link": toml_store::magic_link(&request),
                }),
                Err(error) => error_response(enrollment_message(&error)),
            }
        }
        Some("revoke") => {
            let node_id = body_value
                .get("node_id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            if !params.stores.registry.is_authorized(node_id) {
                return error_response(format!("node {node_id} is not registered"));
            }
            params.stores.presence.remove(node_id);
            match params.stores.registry.revoke(node_id) {
                Ok(_) => serde_json::json!({"ok": true, "node_id": node_id}),
                Err(error) => error_response(format!("revoke: {error}")),
            }
        }
        Some("presence") => {
            let nodes = params
                .stores
                .presence
                .online(now_unix(), params.config.presence_timeout());
            serde_json::json!({
                "ok": true,
                "nodes": nodes.iter().map(presence_json).collect::<Vec<_>>(),
            })
        }
        _ => error_response("unknown admin command"),
    }
}

fn presence_json(entry: &crate::infra::coordination::presence::PresenceEntry) -> serde_json::Value {
    serde_json::json!({
        "node_id": entry.node_id,
        "last_seen": entry.last_seen,
        "addr": serde_json::from_str::<serde_json::Value>(
            &crate::infra::mesh_probe::format_node_addr(&entry.addr)
        )
        .unwrap_or_default(),
    })
}

fn request_json(request: &JoinRequest) -> serde_json::Value {
    serde_json::json!({
        "id": request.id,
        "node_id": request.node_id,
        "state": request.state.as_str(),
        "created_at": request.created_at,
        "expires_at": request.expires_at,
        "joined_at": request.joined_at,
    })
}

fn enrollment_message(error: &EnrollmentError) -> String {
    error.to_string()
}

fn error_response(message: impl Into<String>) -> serde_json::Value {
    serde_json::json!({"ok": false, "error": message.into()})
}

fn hex_to_bytes(text: &str) -> Result<Vec<u8>, ()> {
    if !text.len().is_multiple_of(2) {
        return Err(());
    }
    (0..text.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&text[index..index + 2], 16).map_err(|_| ()))
        .collect()
}

/// Unix seconds, saturating at 0.
pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

/// Signs an admin body, returning `(body, operator, hex signature)`.
pub fn sign_admin_body(key: &SecretKey, body: &str) -> (String, String, String) {
    let signature = key.sign(body.as_bytes());
    let mut hex = String::with_capacity(128);
    use std::fmt::Write;
    for byte in signature.to_bytes() {
        let _ = write!(hex, "{byte:02x}");
    }
    (body.to_string(), key.public().to_string(), hex)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(root: &Path, admin_stdout: bool) -> ServeParams {
        ServeParams {
            config: CoordinationConfig::default(),
            stores: CoordinationStores::open(root).unwrap(),
            admin_stdout,
            relay_url: None,
        }
    }

    #[tokio::test]
    async fn enroll_request_mints_and_admin_stdout_prints_the_link() {
        let root =
            std::env::temp_dir().join(format!("waitagent-coord-server-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let params = params(&root, true);
        let node_id = SecretKey::generate().public().to_string();
        let response = handle_node_frame(
            &params,
            &serde_json::json!({"command": "enroll-request", "node_id": node_id}),
            "anon",
            None,
        )
        .await;
        assert_eq!(response["ok"], true);
        let request_id = response["request_id"].as_str().unwrap();
        let request = params.stores.requests.load(request_id).unwrap();
        assert_eq!(request.node_id, node_id);
        crate::infra::best_effort::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn join_redeems_approved_request_and_registers_the_node() {
        let root =
            std::env::temp_dir().join(format!("waitagent-coord-join-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let params = params(&root, true);
        let node_key = SecretKey::generate();
        let node_id = node_key.public().to_string();
        let response = handle_node_frame(
            &params,
            &serde_json::json!({"command": "enroll-request", "node_id": node_id}),
            "anon",
            None,
        )
        .await;
        let request_id = response["request_id"].as_str().unwrap().to_string();

        let denied = handle_node_frame(
            &params,
            &serde_json::json!({"command": "join", "node_id": node_id, "token": "f00ba4"}),
            "anon",
            None,
        )
        .await;
        assert_eq!(denied["ok"], false);

        let operator = SecretKey::generate();
        let config = CoordinationConfig {
            operators: vec![operator.public().to_string()],
            ..CoordinationConfig::default()
        };
        let params = ServeParams {
            config,
            stores: params.stores,
            admin_stdout: true,
            relay_url: None,
        };
        let body = format!("{{\"command\":\"approve\",\"request_id\":\"{request_id}\"}}");
        let (body, operator_id, signature) = sign_admin_body(&operator, &body);
        let approved = handle_admin_frame(
            &params,
            &serde_json::json!({"body": body, "operator": operator_id, "signature": signature}),
        );
        assert_eq!(approved["ok"], true);
        let token = approved["link"]
            .as_str()
            .unwrap()
            .split("token=")
            .nth(1)
            .unwrap()
            .to_string();

        let joined = handle_node_frame(
            &params,
            &serde_json::json!({"command": "join", "node_id": node_id, "token": token}),
            "anon",
            None,
        )
        .await;
        assert_eq!(joined["ok"], true);
        assert!(params.stores.registry.is_authorized(&node_id));

        let replay = handle_node_frame(
            &params,
            &serde_json::json!({"command": "join", "node_id": node_id, "token": token}),
            "anon",
            None,
        )
        .await;
        assert_eq!(replay["ok"], false);
        crate::infra::best_effort::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn admin_frame_rejects_bad_signature_and_unknown_operator() {
        let root =
            std::env::temp_dir().join(format!("waitagent-coord-admin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let operator = SecretKey::generate();
        let config = CoordinationConfig {
            operators: vec![operator.public().to_string()],
            ..CoordinationConfig::default()
        };
        let params = ServeParams {
            config,
            stores: CoordinationStores::open(&root).unwrap(),
            admin_stdout: true,
            relay_url: None,
        };
        let body = "{\"command\":\"requests\"}";
        let (_, operator_id, signature) = sign_admin_body(&operator, body);
        let stranger = SecretKey::generate();
        let rejected = handle_admin_frame(
            &params,
            &serde_json::json!({"body": body, "operator": stranger.public().to_string(), "signature": signature}),
        );
        assert_eq!(rejected["ok"], false);
        let tampered = handle_admin_frame(
            &params,
            &serde_json::json!({"body": "{\"command\":\"revoke\",\"node_id\":\"x\"}", "operator": operator_id, "signature": signature}),
        );
        assert_eq!(tampered["ok"], false);
        crate::infra::best_effort::remove_dir_all(&root);
    }

    #[test]
    fn sign_admin_body_round_trips_through_verification() {
        let key = SecretKey::generate();
        let (body, operator, signature) = sign_admin_body(&key, "{\"command\":\"requests\"}");
        let public_key = operator.parse::<iroh::PublicKey>().unwrap();
        let bytes = hex_to_bytes(&signature).unwrap();
        let array = <[u8; 64]>::try_from(bytes.as_slice()).unwrap();
        assert!(public_key
            .verify(body.as_bytes(), &iroh::Signature::from_bytes(&array))
            .is_ok());
    }

    #[tokio::test]
    async fn node_responses_announce_the_embedded_relay_url() {
        let root =
            std::env::temp_dir().join(format!("waitagent-coord-relayurl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let params = ServeParams {
            relay_url: Some("http://192.168.1.20:7479/".to_string()),
            ..params(&root, true)
        };
        let node_id = SecretKey::generate().public().to_string();
        let enrolled = handle_node_frame(
            &params,
            &serde_json::json!({"command": "enroll-request", "node_id": node_id}),
            "anon",
            None,
        )
        .await;
        assert_eq!(enrolled["relay_url"], "http://192.168.1.20:7479/");
        crate::infra::best_effort::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn heartbeat_records_presence_only_for_registered_nodes() {
        let root =
            std::env::temp_dir().join(format!("waitagent-coord-heartbeat-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let params = params(&root, true);
        let node_key = SecretKey::generate();
        let node_id = node_key.public().to_string();
        params
            .stores
            .registry
            .register(&node_id, now_unix())
            .unwrap();
        let addr =
            crate::infra::mesh_probe::format_node_addr(&iroh::EndpointAddr::new(node_key.public()));
        let frame = serde_json::json!({"command": "heartbeat", "node_id": node_id, "addr": addr});

        let stranger = handle_node_frame(&params, &frame, "someone-else", None).await;
        assert_eq!(stranger["ok"], false);

        let unregistered = handle_node_frame(
            &params,
            &serde_json::json!({"command": "heartbeat", "node_id": SecretKey::generate().public().to_string(), "addr": addr}),
            &SecretKey::generate().public().to_string(),
            None,
        )
        .await;
        assert_eq!(unregistered["ok"], false);

        let observed: std::net::SocketAddr = "192.168.1.30:9000".parse().unwrap();
        let ok = handle_node_frame(&params, &frame, &node_id, Some(observed)).await;
        assert_eq!(ok["ok"], true);
        let entry = params.stores.presence.get(&node_id).unwrap();
        assert_eq!(entry.last_seen, now_unix());
        assert!(entry
            .addr
            .addrs
            .iter()
            .any(|a| matches!(a, iroh::TransportAddr::Ip(ip) if *ip == observed)));
        crate::infra::best_effort::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn resolve_returns_the_registered_addr_only_to_authorized_nodes() {
        let root =
            std::env::temp_dir().join(format!("waitagent-coord-resolve-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let params = params(&root, true);
        let node_key = SecretKey::generate();
        let node_id = node_key.public().to_string();
        params
            .stores
            .registry
            .register(&node_id, now_unix())
            .unwrap();
        params.stores.presence.record(
            node_id.clone(),
            crate::infra::mesh_probe::parse_node_addr(
                &serde_json::json!({
                    "node_id": node_id,
                    "relay_url": "http://192.168.1.20:7479/",
                    "direct_addresses": ["192.168.1.30:7479"],
                })
                .to_string(),
            )
            .unwrap(),
            None,
            now_unix(),
        );
        let frame = serde_json::json!({"command": "resolve", "node_id": node_id});

        let outsider = handle_node_frame(&params, &frame, "outsider", None).await;
        assert_eq!(outsider["ok"], false);

        params
            .stores
            .registry
            .register("outsider", now_unix())
            .unwrap();
        let resolved = handle_node_frame(&params, &frame, "outsider", None).await;
        assert_eq!(resolved["ok"], true);
        let addr =
            crate::infra::mesh_probe::parse_node_addr(resolved["node_addr"].as_str().unwrap())
                .unwrap();
        assert_eq!(addr.id.to_string(), node_id);
        assert!(addr
            .addrs
            .iter()
            .any(|a| matches!(a, iroh::TransportAddr::Relay(url) if url.as_str() == "http://192.168.1.20:7479/")));
        crate::infra::best_effort::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn admin_presence_lists_online_nodes() {
        let root =
            std::env::temp_dir().join(format!("waitagent-coord-presence-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let operator = SecretKey::generate();
        let params = ServeParams {
            config: CoordinationConfig {
                operators: vec![operator.public().to_string()],
                ..CoordinationConfig::default()
            },
            stores: CoordinationStores::open(&root).unwrap(),
            admin_stdout: true,
            relay_url: None,
        };
        let node_key = SecretKey::generate();
        let node_id = node_key.public().to_string();
        params.stores.presence.record(
            node_id,
            iroh::EndpointAddr::new(node_key.public()),
            None,
            now_unix(),
        );
        let body = "{\"command\":\"presence\"}";
        let (_, operator_id, signature) = sign_admin_body(&operator, body);
        let response = handle_admin_frame(
            &params,
            &serde_json::json!({"body": body, "operator": operator_id, "signature": signature}),
        );
        assert_eq!(response["ok"], true);
        let nodes = response["nodes"].as_array().unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0]["node_id"], node_key.public().to_string());
        crate::infra::best_effort::remove_dir_all(&root);
    }
}
