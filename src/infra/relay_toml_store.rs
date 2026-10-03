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
use crate::infra::relay_join::parse_relay_address;

/// Default node→relay heartbeat cadence, in seconds (three missed beats fit
/// inside the relay's 30s default eviction window). Single source for the
/// store default and the relay client's [`Duration`](std::time::Duration)
/// constant.
pub const DEFAULT_RELAY_HEARTBEAT_INTERVAL_SECS: u64 = 10;

/// The pinned relay: where this node enrolls and which TLS identity to trust.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RelayTomlConfig {
    /// Relay address with explicit port (`host:port`).
    pub address: String,
    /// Lowercase hex SHA-256 SPKI fingerprint of the relay certificate.
    pub relay_fingerprint: String,
    /// Heartbeat cadence in seconds (1..=300); absent means
    /// [`DEFAULT_RELAY_HEARTBEAT_INTERVAL_SECS`]. Written by operators/tools;
    /// `relay join` leaves it unset.
    pub heartbeat_interval_secs: Option<u64>,
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
    if let Some(secs) = config.heartbeat_interval_secs {
        out.push_str("heartbeat_interval_secs = ");
        out.push_str(&secs.to_string());
        out.push('\n');
    }
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
    let mut heartbeat_interval_secs: Option<u64> = None;
    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = parse_key_value(line)?;
        match key.as_str() {
            "address" => address = Some(value),
            "relay_fingerprint" => relay_fingerprint = Some(value),
            "heartbeat_interval_secs" => {
                heartbeat_interval_secs = Some(parse_heartbeat(&value)?);
            }
            other => {
                return Err(RelayTomlStoreError::new(format!(
                    "unknown relay.toml field `{other}`"
                )));
            }
        }
    }
    let address = address.ok_or_else(|| RelayTomlStoreError::new("relay.toml lacks `address`"))?;
    validate_address(&address)?;
    let relay_fingerprint = relay_fingerprint
        .ok_or_else(|| RelayTomlStoreError::new("relay.toml lacks `relay_fingerprint`"))?;
    let relay_fingerprint = normalize_fingerprint(&relay_fingerprint)?;
    Ok(RelayTomlConfig {
        address,
        relay_fingerprint,
        heartbeat_interval_secs,
    })
}

fn validate_address(address: &str) -> Result<(), RelayTomlStoreError> {
    parse_relay_address(address).map(|_| ()).map_err(|error| {
        RelayTomlStoreError::new(format!(
            "relay.toml `address` is not a valid host[:port] ({error}); fix or re-run `relay join`"
        ))
    })
}

/// Fingerprints compare case-insensitively across the relay protocol; the
/// store normalizes to lowercase so consumers get one shape.
fn normalize_fingerprint(fingerprint: &str) -> Result<String, RelayTomlStoreError> {
    if fingerprint.len() != 64 || !fingerprint.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return Err(RelayTomlStoreError::new(
            "relay.toml `relay_fingerprint` must be 64 hex characters (the relay certificate fingerprint); fix or re-run `relay join`".to_string(),
        ));
    }
    Ok(fingerprint.to_lowercase())
}

fn parse_heartbeat(value: &str) -> Result<u64, RelayTomlStoreError> {
    let secs: u64 = value.parse().map_err(|_| {
        RelayTomlStoreError::new(
            "relay.toml `heartbeat_interval_secs` must be a whole number of seconds".to_string(),
        )
    })?;
    if secs == 0 || secs > 300 {
        return Err(RelayTomlStoreError::new(
            "relay.toml `heartbeat_interval_secs` must be within 1..=300".to_string(),
        ));
    }
    Ok(secs)
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
            heartbeat_interval_secs: None,
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
    fn round_trips_heartbeat_interval() {
        let path = unique_path("heartbeat-round-trip");
        let config = RelayTomlConfig {
            heartbeat_interval_secs: Some(42),
            ..sample()
        };
        config.save(&path).expect("save should succeed");
        let text = fs::read_to_string(&path).expect("file readable");
        assert!(
            text.contains("heartbeat_interval_secs = 42"),
            "heartbeat key is persisted: {text}"
        );

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
    fn rejects_an_unparseable_address() {
        let path = unique_path("bad-address");
        fs::write(
            &path,
            "address = \"host:notaport\"\nrelay_fingerprint = \"7c857a105486f46defdfc06ffc2dbc54531a4cdb25515488565aae15264543e4\"\n",
        )
        .expect("write should succeed");
        let error = RelayTomlConfig::load(&path).expect_err("bad address should fail");
        assert!(
            error.to_string().contains("`address`"),
            "the error names the key: {error}"
        );
        crate::infra::best_effort::remove_file(&path);
    }

    #[test]
    fn rejects_a_short_fingerprint() {
        let path = unique_path("short-fingerprint");
        fs::write(&path, "address = \"x:1\"\nrelay_fingerprint = \"abcd\"\n")
            .expect("write should succeed");
        let error = RelayTomlConfig::load(&path).expect_err("short fingerprint should fail");
        assert!(
            error.to_string().contains("`relay_fingerprint`")
                && error.to_string().contains("64 hex"),
            "the error names the key and the fix: {error}"
        );
        crate::infra::best_effort::remove_file(&path);
    }

    #[test]
    fn normalizes_an_uppercase_fingerprint() {
        let path = unique_path("uppercase-fingerprint");
        let uppercased = "7C857A105486F46DEFDFC06FFC2DBC54531A4CDB25515488565AAE15264543E4";
        fs::write(
            &path,
            format!("address = \"x:1\"\nrelay_fingerprint = \"{uppercased}\"\n"),
        )
        .expect("write should succeed");
        let loaded = RelayTomlConfig::load(&path).expect("load should succeed");
        assert_eq!(
            loaded.map(|config| config.relay_fingerprint),
            Some(uppercased.to_lowercase())
        );
        crate::infra::best_effort::remove_file(&path);
    }

    #[test]
    fn rejects_out_of_range_heartbeats() {
        for (name, value) in [("heartbeat-zero", "0"), ("heartbeat-huge", "301")] {
            let path = unique_path(name);
            fs::write(
                &path,
                format!(
                    "address = \"x:1\"\nrelay_fingerprint = \"7c857a105486f46defdfc06ffc2dbc54531a4cdb25515488565aae15264543e4\"\nheartbeat_interval_secs = {value}\n"
                ),
            )
            .expect("write should succeed");
            let error = RelayTomlConfig::load(&path).expect_err("bad heartbeat should fail");
            assert!(
                error.to_string().contains("`heartbeat_interval_secs`"),
                "the error names the key: {error}"
            );
            crate::infra::best_effort::remove_file(&path);
        }
    }

    #[test]
    fn escapes_special_characters() {
        let path = unique_path("escapes");
        let config = RelayTomlConfig {
            address: "ho\"st\\name\n:7475".to_string(),
            ..sample()
        };
        config.save(&path).expect("save should succeed");
        let loaded = RelayTomlConfig::load(&path).expect("load should succeed");
        assert_eq!(loaded, Some(config));
        crate::infra::best_effort::remove_file(&path);
    }
}
