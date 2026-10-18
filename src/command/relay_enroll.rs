//! `waitagent relay invite` / `relay remove` / `relay join`: the enrollment
//! lifecycle commands (docs/relay-design.md 身份认证与入网). invite and
//! remove speak the relay's local admin socket like status/shutdown; join
//! runs the token-authenticated enrollment session and pins the relay
//! identity into `~/.waitagent/relay.toml`.

use crate::cli::{
    RelayInviteCommand, RelayJoinCommand, RelayJoinRequestCommand, RelayRemoveCommand,
    RemoteNetworkConfig,
};
use crate::command::relay_serve::admin_request;
use crate::error::AppError;
use crate::infra::node_credentials::{self, NodeCredentialPaths};
use crate::infra::relay_join;
use crate::infra::relay_toml_store::RelayTomlConfig;
use crate::lifecycle::LifecycleError;

/// Extracts the value of a `key: value` line from an admin response message.
fn message_field<'a>(message: &'a str, key: &str) -> Option<&'a str> {
    message
        .lines()
        .find_map(|line| line.strip_prefix(key))
        .map(str::trim)
}

/// Prints an admin response body; errors exit with the relay's own
/// message so the operator sees the refusal reason.
fn print_admin_response(body: &str) -> Result<(), AppError> {
    let response: serde_json::Value = serde_json::from_str(body).map_err(|error| {
        protocol_error(format!(
            "admin response was not json: {error}; body: {body}"
        ))
    })?;
    if response["ok"] == false {
        return Err(protocol_error(format!(
            "relay admin refused the request: {}",
            response["error"].as_str().unwrap_or(body)
        )));
    }
    if let Some(message) = response["message"].as_str() {
        println!("{message}");
    } else {
        println!("{body}");
    }
    Ok(())
}

fn protocol_error(message: impl Into<String>) -> AppError {
    AppError::Lifecycle(LifecycleError::Protocol(message.into()))
}

pub fn run_invite(command: RelayInviteCommand) -> Result<(), AppError> {
    let mut body = String::from("{\"command\":\"invite\"");
    if let Some(ttl) = &command.ttl_secs {
        let ttl_secs = ttl.parse::<u64>().map_err(|_| {
            protocol_error(format!("invalid --ttl value {ttl:?}: expected seconds"))
        })?;
        body.push_str(&format!(",\"ttl_secs\":{ttl_secs}"));
    }
    if command.deploy {
        body.push_str(",\"deploy\":true");
    }
    body.push('}');

    let response = admin_request(&command.listen, &body)?;
    print_admin_response(&response)?;

    // Best-effort clipboard copy: a headless relay host has no clipboard,
    // and the token is still fully visible above.
    let token = serde_json::from_str::<serde_json::Value>(&response)
        .ok()
        .and_then(|value| value["message"].as_str().map(str::to_string))
        .and_then(|message| message_field(&message, "token: ").map(str::to_string));
    if let Some(token) = token {
        match arboard::Clipboard::new().and_then(|mut clipboard| clipboard.set_text(token)) {
            Ok(()) => println!("(token copied to the clipboard)"),
            Err(_) => println!("(clipboard unavailable, copy the token manually)"),
        }
    }
    Ok(())
}

pub fn run_remove(command: RelayRemoveCommand) -> Result<(), AppError> {
    let body = format!(
        "{{\"command\":\"remove\",\"fingerprint\":\"{}\"}}",
        command.fingerprint
    );
    let response = admin_request(&command.listen, &body)?;
    print_admin_response(&response)
}

pub fn run_join(command: RelayJoinCommand, network: &RemoteNetworkConfig) -> Result<(), AppError> {
    let proxy = resolve_join_proxy(command.via_proxy)?;
    let token = match &command.link {
        // Issue #208: redeem the self-service link for a one-time enroll
        // token, then fall through to the standard enrollment protocol.
        Some(link) => redeem_enroll_link(link, &command.address, network)?,
        None => command.token.clone(),
    };
    let outcome = join_and_pin_relay(&command.address, &token, network, proxy)?;
    println!("relay fingerprint: {}", outcome.relay_fingerprint);
    println!("pinned to: {}", outcome.toml_path.display());
    println!("join complete: this node is enrolled and whitelisted by the relay");
    Ok(())
}

/// `waitagent relay join-request <relay:port>` (issue #208): the
/// credential-less start of the self-service enrollment. Posts this
/// device's credential fingerprint to the relay's dashboard; the relay
/// mails the administrator a one-time, device-bound magic link, and
/// forwarding that mail to the applicant is the approval.
pub fn run_join_request(
    command: RelayJoinRequestCommand,
    network: &RemoteNetworkConfig,
) -> Result<(), AppError> {
    let credentials = NodeCredentialPaths::resolve_overrides(
        network.node_key_path.as_deref(),
        network.node_cert_path.as_deref(),
    );
    // The fingerprint binds the request to THIS device; the relay refuses
    // redemption from anywhere else (issue #208 semantics).
    node_credentials::ensure_credentials(&credentials)
        .map_err(|error| join_request_error(error.to_string()))?;
    let fp = node_credentials::load_cert_fingerprint(&credentials.cert_path)
        .map_err(|error| join_request_error(error.to_string()))?;
    let proxy = resolve_enroll_leg_proxy()?;
    let body = serde_json::json!({ "fp": fp }).to_string();
    let (status, answer) = enroll_api_post(
        &command.address,
        proxy.as_ref(),
        "/auth/join-request",
        &body,
    )?;
    if !(200..300).contains(&status) {
        return Err(join_request_error(format!(
            "the relay refused the enrollment request (HTTP {status}): {answer}"
        )));
    }
    println!("enrollment request mailed to the relay administrator.");
    println!("once the mail is forwarded to you, run:");
    println!("  waitagent relay join <relay> --link <the link from the mail>");
    Ok(())
}

/// Redeems a self-service link (full URL or bare JWT) for a one-time enroll
/// token via `POST /auth/enroll`. The HTTPS legs follow the active proxy
/// profile (issue #208: same local→relay proxy semantics as the join
/// itself; there is no pin yet, so the current active profile decides).
fn redeem_enroll_link(
    link: &str,
    relay_address: &str,
    network: &RemoteNetworkConfig,
) -> Result<String, AppError> {
    let magic = extract_magic_token(link).map_err(join_request_error)?;
    let credentials = NodeCredentialPaths::resolve_overrides(
        network.node_key_path.as_deref(),
        network.node_cert_path.as_deref(),
    );
    let fp = node_credentials::load_cert_fingerprint(&credentials.cert_path)
        .map_err(|error| join_request_error(error.to_string()))?;
    let proxy = resolve_enroll_leg_proxy()?;
    let body = serde_json::json!({ "token": magic, "fp": fp }).to_string();
    let (status, answer) = enroll_api_post(relay_address, proxy.as_ref(), "/auth/enroll", &body)?;
    if !(200..300).contains(&status) {
        return Err(join_request_error(format!(
            "the link was refused (HTTP {status}): {answer}"
        )));
    }
    let parsed: serde_json::Value = serde_json::from_str(&answer).map_err(|error| {
        join_request_error(format!(
            "the relay answered /auth/enroll with no JSON ({error})"
        ))
    })?;
    parsed["token"].as_str().map(str::to_string).ok_or_else(|| {
        join_request_error("the relay answered /auth/enroll without an enroll token")
    })
}

/// Accepts both shapes the operator can paste: a full
/// `.../auth/enroll?token=<jwt>` URL (query parameters after `token=` are
/// ignored) or the bare JWT itself.
fn extract_magic_token(link: &str) -> Result<String, String> {
    let trimmed = link.trim();
    if let Some(query) = trimmed.split_once('?').map(|(_, query)| query) {
        for pair in query.split('&') {
            if let Some(value) = pair.strip_prefix("token=") {
                if !value.is_empty() {
                    return Ok(value.to_string());
                }
            }
        }
        return Err("the link URL carries no token= parameter".to_string());
    }
    if trimmed.matches('.').count() == 2 && !trimmed.contains('/') {
        return Ok(trimmed.to_string());
    }
    Err("expected a full enroll URL or the bare token from the mail".to_string())
}

/// Issue #208: resolves the proxy for the self-service HTTPS legs. No pin
/// exists at request time, so the CURRENT active remote-install proxy
/// profile decides — consistent with the local→relay legs of the join
/// itself. A broken profile fails the command with the actionable message
/// rather than silently falling back (same policy as `--via-proxy`).
fn resolve_enroll_leg_proxy() -> Result<Option<crate::infra::proxy_stream::SshProxy>, AppError> {
    crate::host::ssh::remote_install_proxy_store::load_active_ssh_proxy().map_err(|error| {
        join_request_error(format!(
            "the active proxy profile cannot be used for the enrollment request: {error}"
        ))
    })
}

fn enroll_api_post(
    relay_address: &str,
    proxy: Option<&crate::infra::proxy_stream::SshProxy>,
    path: &str,
    body: &str,
) -> Result<(u16, String), AppError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| {
            AppError::Lifecycle(LifecycleError::Io(
                "build enrollment request runtime".to_string(),
                error,
            ))
        })?;
    runtime
        .block_on(crate::infra::relay_join::post_enroll_api(
            relay_address,
            proxy,
            path,
            body,
        ))
        .map_err(|error| join_request_error(error.to_string()))
}

fn join_request_error(message: impl Into<String>) -> AppError {
    AppError::Lifecycle(LifecycleError::Protocol(
        format!(
            "self-service enrollment failed: {}\n\
             (fall back to the operator-minted token: `waitagent relay invite`, then `waitagent relay join <relay> <token>`)",
            message.into()
        ),
    ))
}

/// Issue #206: resolves the active remote-install proxy profile when the
/// operator asked for a proxied join. A resolution failure refuses the join
/// with the same actionable message the SSH connect uses (issue #201), so a
/// hand-edited proxy profile surfaces instead of being silently ignored.
fn resolve_join_proxy(
    via_proxy: bool,
) -> Result<Option<crate::infra::proxy_stream::SshProxy>, AppError> {
    if !via_proxy {
        return Ok(None);
    }
    crate::host::ssh::remote_install_proxy_store::load_active_ssh_proxy().map_err(|error| {
        protocol_error(format!(
            "via proxy requested but the active proxy profile cannot be used: {error}"
        ))
    })
}

/// Runs the token-authenticated enrollment session and pins the relay
/// identity into `relay.toml` — the shared engine behind `relay join` and
/// the TUI connect flow's relay management (issue #156), so both surfaces
/// enroll with identical semantics. The enrollment cert and the whitelist
/// identity fingerprint are both derived from the resolved credential
/// paths (issue #141): an explicit `--node-key-path`/`--node-cert-path`
/// pair wins over the defaults, exactly like `relay serve` and
/// `__generate-node-credentials`. `proxy` (issue #206) routes the
/// enrollment dial through the active proxy profile and is recorded in
/// the written pin for the persistent link.
pub fn join_and_pin_relay(
    address: &str,
    token: &str,
    network: &RemoteNetworkConfig,
    proxy: Option<crate::infra::proxy_stream::SshProxy>,
) -> Result<crate::infra::relay_join::JoinOutcome, AppError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| {
            AppError::Lifecycle(LifecycleError::Io(
                "build relay join runtime".to_string(),
                error,
            ))
        })?;
    runtime.block_on(async move {
        let credentials = NodeCredentialPaths::resolve_overrides(
            network.node_key_path.as_deref(),
            network.node_cert_path.as_deref(),
        );
        relay_join::join_relay(
            address,
            token,
            &credentials,
            &RelayTomlConfig::default_path(),
            proxy,
        )
        .await
        .map_err(|error| protocol_error(format!("relay join failed: {error}")))
    })
}

/// Best-effort read of the currently pinned relay. The TUI join flow uses
/// it as the fingerprint anchor across a re-join/switch; an unreadable or
/// missing pin yields `None` so a stale file can never block enrollment.
pub fn current_relay_pin() -> Option<RelayTomlConfig> {
    match RelayTomlConfig::load(&RelayTomlConfig::default_path()) {
        Ok(pin) => pin,
        Err(error) => {
            crate::infra::error_log::ERROR_LOG.log(format!(
                "[relay-join] existing relay pin unreadable; mismatch check skipped: {error}"
            ));
            None
        }
    }
}

/// How a completed join reconciles with the pin it replaced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayJoinPinDecision {
    /// No pin existed before, or the same fingerprint was re-pinned: the
    /// join is applied quietly.
    Consistent,
    /// The relay presented a different fingerprint than the pin it
    /// replaced. The caller either restores `previous` and aborts (the
    /// default) or accepts the new identity after an explicit operator
    /// confirmation (`force`).
    PinMismatch {
        /// The pin that was in place before this join overwrote it.
        previous: RelayTomlConfig,
    },
}

/// Compares the freshly pinned fingerprint with the replaced pin. The
/// invite token carries no fingerprint (it is random material, see
/// `relay_enrollment`), so the previously pinned identity is the only
/// local anchor: a silent fingerprint change across a re-join of the
/// "same" relay (or an unattended switch) must surface as a pin mismatch
/// rather than a quiet re-pin (issue #156).
pub fn decide_relay_join_pin(
    previous: Option<&RelayTomlConfig>,
    joined_fingerprint: &str,
    force: bool,
) -> RelayJoinPinDecision {
    match previous {
        Some(previous) if !force && previous.relay_fingerprint != joined_fingerprint => {
            RelayJoinPinDecision::PinMismatch {
                previous: previous.clone(),
            }
        }
        _ => RelayJoinPinDecision::Consistent,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pinned(fingerprint: &str) -> RelayTomlConfig {
        RelayTomlConfig {
            address: "relay.example:7475".to_string(),
            relay_fingerprint: fingerprint.to_string(),
            heartbeat_interval_secs: None,
            use_proxy: false,
        }
    }

    #[test]
    fn extract_magic_token_accepts_urls_and_bare_jwts() {
        assert_eq!(
            extract_magic_token("https://dash.example/auth/enroll?token=aaa.bbb.ccc").unwrap(),
            "aaa.bbb.ccc"
        );
        // Extra query parameters after the token are ignored.
        assert_eq!(
            extract_magic_token("https://dash.example/auth/enroll?token=aaa.bbb.ccc&utm=x")
                .unwrap(),
            "aaa.bbb.ccc"
        );
        assert_eq!(extract_magic_token("aaa.bbb.ccc").unwrap(), "aaa.bbb.ccc");
        // A dash-leading base64url token survives untouched.
        assert_eq!(
            extract_magic_token("-dG9rZW4.sig.part").unwrap(),
            "-dG9rZW4.sig.part"
        );
        assert!(extract_magic_token("https://dash.example/auth/enroll?nope=1").is_err());
        assert!(extract_magic_token("not a token").is_err());
        assert!(extract_magic_token("").is_err());
    }

    #[test]
    fn message_field_extracts_prefixed_lines() {
        let message = "token: abc-123\nexpires_at: 1893456000\nkind: one-time invite token";
        assert_eq!(message_field(message, "token: "), Some("abc-123"));
        assert_eq!(message_field(message, "expires_at: "), Some("1893456000"));
        assert_eq!(message_field(message, "missing: "), None);
    }

    #[test]
    fn first_join_without_a_previous_pin_is_consistent() {
        assert_eq!(
            decide_relay_join_pin(None, "ab".repeat(32).leak(), false),
            RelayJoinPinDecision::Consistent
        );
    }

    #[test]
    fn rejoin_with_the_same_fingerprint_is_consistent() {
        let fingerprint = "ab".repeat(32);
        let previous = pinned(&fingerprint);
        assert_eq!(
            decide_relay_join_pin(Some(&previous), &fingerprint, false),
            RelayJoinPinDecision::Consistent
        );
    }

    #[test]
    fn fingerprint_change_without_force_is_a_pin_mismatch() {
        let previous = pinned(&"ab".repeat(32));
        let decision = decide_relay_join_pin(Some(&previous), &"cd".repeat(32), false);
        assert_eq!(
            decision,
            RelayJoinPinDecision::PinMismatch {
                previous: previous.clone()
            }
        );
    }

    #[test]
    fn forced_join_accepts_the_new_fingerprint() {
        let previous = pinned(&"ab".repeat(32));
        assert_eq!(
            decide_relay_join_pin(Some(&previous), &"cd".repeat(32), true),
            RelayJoinPinDecision::Consistent
        );
    }

    #[test]
    fn join_engine_surfaces_relay_errors_as_app_errors() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        // Credential paths point at throwaway files: the engine calls
        // `ensure_credentials` before dialing, which must not touch the
        // developer's real ~/.waitagent identity.
        let credential_dir = std::env::temp_dir().join(format!(
            "waitagent-test-join-engine-creds-{}",
            std::process::id()
        ));
        let network = RemoteNetworkConfig {
            node_key_path: Some(
                credential_dir
                    .join("node.key")
                    .to_string_lossy()
                    .into_owned(),
            ),
            node_cert_path: Some(
                credential_dir
                    .join("node.crt")
                    .to_string_lossy()
                    .into_owned(),
            ),
            ..RemoteNetworkConfig::default()
        };
        let error = join_and_pin_relay("127.0.0.1:1", "token", &network, None)
            .expect_err("dialing a closed port must fail");
        assert!(
            error.to_string().contains("relay join failed"),
            "the error keeps the relay join context: {error}"
        );
        crate::infra::best_effort::remove_dir_all(&credential_dir);
    }
}
