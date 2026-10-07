//! `waitagent relay invite` / `relay remove` / `relay join`: the enrollment
//! lifecycle commands (docs/relay-design.md 身份认证与入网). invite and
//! remove speak the relay's local admin socket like status/shutdown; join
//! runs the token-authenticated enrollment session and pins the relay
//! identity into `~/.waitagent/relay.toml`.

use crate::cli::{RelayInviteCommand, RelayJoinCommand, RelayRemoveCommand, RemoteNetworkConfig};
use crate::command::relay_serve::admin_request;
use crate::error::AppError;
use crate::infra::node_credentials::NodeCredentialPaths;
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
    let outcome = join_and_pin_relay(&command.address, &command.token, network)?;
    println!("relay fingerprint: {}", outcome.relay_fingerprint);
    println!("pinned to: {}", outcome.toml_path.display());
    println!("join complete: this node is enrolled and whitelisted by the relay");
    Ok(())
}

/// Runs the token-authenticated enrollment session and pins the relay
/// identity into `relay.toml` — the shared engine behind `relay join` and
/// the TUI connect flow's relay management (issue #156), so both surfaces
/// enroll with identical semantics. The enrollment cert and the whitelist
/// identity fingerprint are both derived from the resolved credential
/// paths (issue #141): an explicit `--node-key-path`/`--node-cert-path`
/// pair wins over the defaults, exactly like `relay serve` and
/// `__generate-node-credentials`.
pub fn join_and_pin_relay(
    address: &str,
    token: &str,
    network: &RemoteNetworkConfig,
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
        }
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
        let error = join_and_pin_relay("127.0.0.1:1", "token", &network)
            .expect_err("dialing a closed port must fail");
        assert!(
            error.to_string().contains("relay join failed"),
            "the error keeps the relay join context: {error}"
        );
        crate::infra::best_effort::remove_dir_all(&credential_dir);
    }
}
