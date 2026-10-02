//! The node's pinned relay config: `~/.waitagent/relay.toml`
//! (docs/relay-design.md 身份认证与入网). `relay join` writes the relay
//! address and the certificate fingerprint learned from the token-authenticated
//! enrollment session; the node runtime reads it to pin the relay's TLS
//! identity on every subsequent connection (mismatch → drop).
//!
//! The format is a hand-rolled two-key TOML-line subset, same idiom as
//! `settings_store`: `key = "value"` lines, `#` comments, unknown keys
//! rejected so a stray edit surfaces instead of being silently ignored.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::host::ssh::remote_host_home::waitagent_home;

/// The pinned relay: where this node enrolls and which TLS identity to trust.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayTomlConfig {
    /// Relay address with explicit port (`host:port`).
    pub address: String,
    /// Lowercase hex SHA-256 SPKI fingerprint of the relay certificate.
    pub relay_fingerprint: String,
}

impl RelayTomlConfig {
    /// Returns the default path: `waitagent_home()/relay.toml`.
    pub fn default_path() -> PathBuf {
        waitagent_home().join("relay.toml")
    }

    /// Writes the config to `path`, creating the parent directory when
    /// missing.
    pub fn save(&self, path: &Path) -> Result<(), RelayTomlStoreError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(RelayTomlStoreError::io)?;
        }
        fs::write(path, serialize_relay_toml(self)).map_err(RelayTomlStoreError::io)
    }

    /// Loads the config from `path`; a missing file yields `None`.
    // Consumed by the node-side relay client when it connects through the
    // relay (later slice); today only tests and the join flow read
    // relay.toml back.
    #[allow(dead_code)]
    pub fn load(path: &Path) -> Result<Option<Self>, RelayTomlStoreError> {
        if !path.is_file() {
            return Ok(None);
        }
        let text = fs::read_to_string(path).map_err(RelayTomlStoreError::io)?;
        parse_relay_toml(&text).map(Some)
    }
}

/// Errors of the relay.toml store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayTomlStoreError {
    message: String,
}

impl RelayTomlStoreError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    fn io(error: io::Error) -> Self {
        Self::new(error.to_string())
    }
}

impl fmt::Display for RelayTomlStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for RelayTomlStoreError {}

fn serialize_relay_toml(config: &RelayTomlConfig) -> String {
    let mut out = String::new();
    out.push_str("# WaitAgent pinned relay (written by `relay join`)\n");
    push_string(&mut out, "address", &config.address);
    push_string(&mut out, "relay_fingerprint", &config.relay_fingerprint);
    out
}

fn push_string(out: &mut String, key: &str, value: &str) {
    out.push_str(key);
    out.push_str(" = \"");
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other => out.push(other),
        }
    }
    out.push_str("\"\n");
}

fn parse_relay_toml(text: &str) -> Result<RelayTomlConfig, RelayTomlStoreError> {
    let mut address: Option<String> = None;
    let mut relay_fingerprint: Option<String> = None;
    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = parse_key_value(line)?;
        match key.as_str() {
            "address" => address = Some(value),
            "relay_fingerprint" => relay_fingerprint = Some(value),
            other => {
                return Err(RelayTomlStoreError::new(format!(
                    "unknown relay.toml field `{other}`"
                )));
            }
        }
    }
    Ok(RelayTomlConfig {
        address: address.ok_or_else(|| RelayTomlStoreError::new("relay.toml lacks `address`"))?,
        relay_fingerprint: relay_fingerprint
            .ok_or_else(|| RelayTomlStoreError::new("relay.toml lacks `relay_fingerprint`"))?,
    })
}

fn parse_key_value(line: &str) -> Result<(String, String), RelayTomlStoreError> {
    let Some((key, value)) = line.split_once('=') else {
        return Err(RelayTomlStoreError::new(format!(
            "invalid relay.toml line `{line}`"
        )));
    };
    let key = key.trim().to_string();
    let value = value.trim();
    if value.starts_with('"') {
        return Ok((key, parse_quoted(value)?));
    }
    Ok((key, value.to_string()))
}

fn parse_quoted(value: &str) -> Result<String, RelayTomlStoreError> {
    if !value.ends_with('"') || value.len() < 2 {
        return Err(RelayTomlStoreError::new("unterminated relay.toml string"));
    }
    let mut out = String::new();
    let mut chars = value[1..value.len() - 1].chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('\\') => out.push('\\'),
            Some('"') => out.push('"'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "waitagent-relay-toml-{name}-{}-{}.toml",
            std::process::id(),
            std::thread::current()
                .name()
                .unwrap_or("test")
                .replace(":", "_")
        ))
    }

    fn sample() -> RelayTomlConfig {
        RelayTomlConfig {
            address: "relay.example:7475".to_string(),
            relay_fingerprint: "7c857a105486f46defdfc06ffc2dbc54531a4cdb25515488565aae15264543e4"
                .to_string(),
        }
    }

    #[test]
    fn default_path_uses_waitagent_home() {
        let path = RelayTomlConfig::default_path();
        assert!(path.ends_with(PathBuf::from(".waitagent/relay.toml")));
    }

    #[test]
    fn round_trips_relay_toml() {
        let path = unique_path("round-trip");
        let config = sample();
        config.save(&path).expect("save should succeed");

        let loaded = RelayTomlConfig::load(&path).expect("load should succeed");
        assert_eq!(loaded, Some(config));
        crate::infra::best_effort::remove_file(&path);
    }

    #[test]
    fn load_missing_file_yields_none() {
        let path = unique_path("missing");
        assert_eq!(RelayTomlConfig::load(&path).expect("ok"), None);
    }

    #[test]
    fn rejects_unknown_keys() {
        let path = unique_path("unknown-key");
        fs::write(
            &path,
            "address = \"x:1\"\nrelay_fingerprint = \"ab\"\nsurprise = 1\n",
        )
        .expect("write should succeed");
        let error = RelayTomlConfig::load(&path).expect_err("unknown key should fail");
        assert!(error.to_string().contains("unknown relay.toml field"));
        crate::infra::best_effort::remove_file(&path);
    }

    #[test]
    fn rejects_missing_keys() {
        let path = unique_path("missing-key");
        fs::write(&path, "address = \"x:1\"\n").expect("write should succeed");
        let error = RelayTomlConfig::load(&path).expect_err("missing key should fail");
        assert!(error.to_string().contains("lacks `relay_fingerprint`"));
        crate::infra::best_effort::remove_file(&path);
    }

    #[test]
    fn escapes_special_characters() {
        let path = unique_path("escapes");
        let config = RelayTomlConfig {
            address: "ho\"st\\name\n:7475".to_string(),
            relay_fingerprint: "ab\tcd".to_string(),
        };
        config.save(&path).expect("save should succeed");
        let loaded = RelayTomlConfig::load(&path).expect("load should succeed");
        assert_eq!(loaded, Some(config));
        crate::infra::best_effort::remove_file(&path);
    }
}
