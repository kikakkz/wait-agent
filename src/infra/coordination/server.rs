use std::path::Path;
use std::sync::Arc;

use iroh::{Endpoint, RelayMode, SecretKey};

use crate::infra::coordination::enrollment::{
    EnrollmentError, JoinRequest, RegistryStore, RequestState, RequestStore,
};
use crate::infra::coordination::toml_store::{self, CoordinationConfig};
use crate::infra::coordination::{ADMIN_ALPN, NODE_ALPN};

/// The request and registry stores behind one handle.
pub struct CoordinationStores {
    pub requests: RequestStore,
    pub registry: RegistryStore,
}

impl CoordinationStores {
    /// Opens both stores under `root`.
    pub fn open(root: &Path) -> std::io::Result<Self> {
        Ok(Self {
            requests: RequestStore::new(root)?,
            registry: RegistryStore::new(root)?,
        })
    }
}

/// Runtime inputs of the coordination serve loop.
pub struct ServeParams {
    pub config: CoordinationConfig,
    pub stores: CoordinationStores,
    pub admin_stdout: bool,
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
    println!("node_id: {}", endpoint.id());
    println!(
        "node_addr: {}",
        crate::infra::mesh_probe::format_node_addr(&node_addr)
    );
    println!("coordination listening; ctrl-c to stop");
    let params = Arc::new(params);
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
    let conn = connecting.await.map_err(|error| error.to_string())?;
    let (mut send, mut recv) = conn.accept_bi().await.map_err(|error| error.to_string())?;
    let frame = read_frame(&mut recv).await?;
    let response = handle_node_frame(&params, &frame).await;
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
pub async fn handle_node_frame(
    params: &ServeParams,
    frame: &serde_json::Value,
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
                        Ok(()) => serde_json::json!({
                            "ok": true,
                            "request_id": request.id,
                            "expires_at": request.expires_at,
                        }),
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
                    Ok(()) => {
                        serde_json::json!({"ok": true, "node_id": node_id, "request_id": request.id})
                    }
                    Err(error) => error_response(format!("register node: {error}")),
                },
                Err(error) => error_response(enrollment_message(&error)),
            }
        }
        _ => error_response("unknown command"),
    }
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
            match params.stores.registry.revoke(node_id) {
                Ok(_) => serde_json::json!({"ok": true, "node_id": node_id}),
                Err(error) => error_response(format!("revoke: {error}")),
            }
        }
        _ => error_response("unknown admin command"),
    }
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
        )
        .await;
        let request_id = response["request_id"].as_str().unwrap().to_string();

        let denied = handle_node_frame(
            &params,
            &serde_json::json!({"command": "join", "node_id": node_id, "token": "f00ba4"}),
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
        )
        .await;
        assert_eq!(joined["ok"], true);
        assert!(params.stores.registry.is_authorized(&node_id));

        let replay = handle_node_frame(
            &params,
            &serde_json::json!({"command": "join", "node_id": node_id, "token": token}),
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
}
