//! `waitagent relay invite` / `relay remove` / `relay join`: the enrollment
//! lifecycle commands (docs/relay-design.md 身份认证与入网). invite and
//! remove speak the relay's local admin socket like status/shutdown; join
//! runs the token-authenticated enrollment session and pins the relay
//! identity into `~/.waitagent/relay.toml`.

use crate::cli::{RelayInviteCommand, RelayJoinCommand, RelayRemoveCommand};
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

pub fn run_join(command: RelayJoinCommand) -> Result<(), AppError> {
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
        let outcome = relay_join::join_relay(
            &command.address,
            &command.token,
            &NodeCredentialPaths::default_paths(),
            &RelayTomlConfig::default_path(),
        )
        .await
        .map_err(|error| protocol_error(format!("relay join failed: {error}")))?;
        println!("relay fingerprint: {}", outcome.relay_fingerprint);
        println!("pinned to: {}", outcome.toml_path.display());
        println!("join complete: this node is enrolled and whitelisted by the relay");
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_field_extracts_prefixed_lines() {
        let message = "token: abc-123\nexpires_at: 1893456000\nkind: one-time invite token";
        assert_eq!(message_field(message, "token: "), Some("abc-123"));
        assert_eq!(message_field(message, "expires_at: "), Some("1893456000"));
        assert_eq!(message_field(message, "missing: "), None);
    }
}
