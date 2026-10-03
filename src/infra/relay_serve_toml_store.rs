//! The relay's own `relay.toml`: static configuration for `relay serve`
//! (~/.waitagent/relay.toml). Hand-rolled two-column TOML-lines, same idiom
//! as `relay_toml_store` / `settings_store`: `key = value` lines, `#`
//! comments, unknown keys rejected naming the key.
//!
//! Every key is optional — the file may set any subset and `relay serve`
//! fills the rest from CLI flags and defaults (precedence: CLI > file >
//! default). Keys:
//!
//! ```text
//! listen                                SocketAddr, e.g. "0.0.0.0:7475"
//! admin_socket                          absolute UDS path for the admin
//!                                       listener (Unix only; Windows
//!                                       refuses to start when set)
//! whitelist_dir                         authorized_nodes directory
//! token_invite_ttl_secs                 enrollment invite TTL, seconds (> 0)
//! token_deploy_ttl_secs                 enrollment deploy TTL, seconds (> 0)
//! heartbeat_offline_after_secs          silence before eviction, sec (> 0)
//! register_timeout_secs                 first-frame register deadline, sec
//!                                       (> 0)
//! capacity_max_nodes                    connection-table node cap (> 0)
//! capacity_max_streams                  routed-stream cap (> 0)
//! capacity_max_throughput_bytes_per_sec forwarded-bytes admission rate
//!                                       (> 0)
//! ```

use std::fmt;
use std::fs;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use crate::host::ssh::remote_host_home::waitagent_home;

/// Static relay-serve configuration loaded from `relay.toml`. `None` fields
/// fall back to CLI flags and then hard defaults.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RelayServeTomlConfig {
    /// TLS listener address.
    pub listen: Option<SocketAddr>,
    /// Admin socket path (absolute; Unix only).
    pub admin_socket: Option<PathBuf>,
    /// Whitelist directory (one file per authorized node fingerprint).
    pub whitelist_dir: Option<PathBuf>,
    /// Enrollment invite-token TTL.
    pub token_invite_ttl_secs: Option<u64>,
    /// Enrollment deploy-token TTL.
    pub token_deploy_ttl_secs: Option<u64>,
    /// Silence after which a registered node is evicted.
    pub heartbeat_offline_after_secs: Option<u64>,
    /// First-frame register deadline for a fresh link.
    pub register_timeout_secs: Option<u64>,
    /// Connection-table node cap.
    pub capacity_max_nodes: Option<usize>,
    /// Routed-stream cap.
    pub capacity_max_streams: Option<usize>,
    /// Forwarded-bytes admission rate per second.
    pub capacity_max_throughput_bytes_per_sec: Option<u64>,
}

/// The default path: `waitagent_home()/relay.toml` — the same file the node
/// side pins its relay into; a host runs either the node or the relay, so
/// the shared name stays unambiguous.
pub(crate) fn default_path() -> PathBuf {
    waitagent_home().join("relay.toml")
}

impl RelayServeTomlConfig {
    /// Loads the config from `path`; a missing file yields `None`.
    pub fn load(path: &Path) -> Result<Option<Self>, RelayServeTomlStoreError> {
        if !path.is_file() {
            return Ok(None);
        }
        let text = fs::read_to_string(path).map_err(RelayServeTomlStoreError::io)?;
        parse_serve_toml(&text).map(Some)
    }

    /// Writes every set key to `path`, creating the parent directory when
    /// missing. Round-trips with [`RelayServeTomlConfig::load`].
    #[allow(dead_code)]
    // Consumed by the store's roundtrip tests today; operator tooling that
    // writes relay.toml lands with the serve-config follow-up slices.
    pub fn save(&self, path: &Path) -> Result<(), RelayServeTomlStoreError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(RelayServeTomlStoreError::io)?;
        }
        fs::write(path, serialize_serve_toml(self)).map_err(RelayServeTomlStoreError::io)
    }
}

/// Errors of the serve-config store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayServeTomlStoreError {
    message: String,
}

impl RelayServeTomlStoreError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    fn io(error: io::Error) -> Self {
        Self::new(error.to_string())
    }
}

impl fmt::Display for RelayServeTomlStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for RelayServeTomlStoreError {}

#[allow(dead_code)]
// Serialization helpers back `save` (test-consumed today).
fn serialize_serve_toml(config: &RelayServeTomlConfig) -> String {
    let mut out = String::new();
    out.push_str("# WaitAgent relay serve configuration\n");
    if let Some(listen) = config.listen {
        out.push_str("listen = \"");
        out.push_str(&listen.to_string());
        out.push_str("\"\n");
    }
    if let Some(path) = &config.admin_socket {
        push_string(&mut out, "admin_socket", &path.to_string_lossy());
    }
    if let Some(path) = &config.whitelist_dir {
        push_string(&mut out, "whitelist_dir", &path.to_string_lossy());
    }
    if let Some(secs) = config.token_invite_ttl_secs {
        push_secs(&mut out, "token_invite_ttl_secs", secs);
    }
    if let Some(secs) = config.token_deploy_ttl_secs {
        push_secs(&mut out, "token_deploy_ttl_secs", secs);
    }
    if let Some(secs) = config.heartbeat_offline_after_secs {
        push_secs(&mut out, "heartbeat_offline_after_secs", secs);
    }
    if let Some(secs) = config.register_timeout_secs {
        push_secs(&mut out, "register_timeout_secs", secs);
    }
    if let Some(value) = config.capacity_max_nodes {
        push_secs(&mut out, "capacity_max_nodes", value as u64);
    }
    if let Some(value) = config.capacity_max_streams {
        push_secs(&mut out, "capacity_max_streams", value as u64);
    }
    if let Some(value) = config.capacity_max_throughput_bytes_per_sec {
        push_secs(&mut out, "capacity_max_throughput_bytes_per_sec", value);
    }
    out
}

fn push_secs(out: &mut String, key: &str, value: u64) {
    out.push_str(key);
    out.push_str(" = ");
    out.push_str(&value.to_string());
    out.push('\n');
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

fn parse_serve_toml(text: &str) -> Result<RelayServeTomlConfig, RelayServeTomlStoreError> {
    let mut config = RelayServeTomlConfig::default();
    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = parse_key_value(line)?;
        match key.as_str() {
            "listen" => {
                config.listen = Some(value.parse::<SocketAddr>().map_err(|_| {
                    RelayServeTomlStoreError::new(format!(
                        "relay.toml `listen` must be a socket address like \"0.0.0.0:7475\", got {value:?}"
                    ))
                })?);
            }
            "admin_socket" => {
                let path = PathBuf::from(&value);
                if !path.is_absolute() {
                    return Err(RelayServeTomlStoreError::new(format!(
                        "relay.toml `admin_socket` must be an absolute path, got {value:?}"
                    )));
                }
                config.admin_socket = Some(path);
            }
            "whitelist_dir" => config.whitelist_dir = Some(PathBuf::from(value)),
            "token_invite_ttl_secs" => {
                config.token_invite_ttl_secs = Some(parse_positive_secs(&key, &value)?);
            }
            "token_deploy_ttl_secs" => {
                config.token_deploy_ttl_secs = Some(parse_positive_secs(&key, &value)?);
            }
            "heartbeat_offline_after_secs" => {
                config.heartbeat_offline_after_secs = Some(parse_positive_secs(&key, &value)?);
            }
            "register_timeout_secs" => {
                config.register_timeout_secs = Some(parse_positive_secs(&key, &value)?);
            }
            "capacity_max_nodes" => {
                config.capacity_max_nodes = Some(parse_positive_usize(&key, &value)?);
            }
            "capacity_max_streams" => {
                config.capacity_max_streams = Some(parse_positive_usize(&key, &value)?);
            }
            "capacity_max_throughput_bytes_per_sec" => {
                config.capacity_max_throughput_bytes_per_sec =
                    Some(parse_positive_secs(&key, &value)?);
            }
            other => {
                return Err(RelayServeTomlStoreError::new(format!(
                    "unknown relay.toml field `{other}`"
                )));
            }
        }
    }
    Ok(config)
}

fn parse_positive_secs(key: &str, value: &str) -> Result<u64, RelayServeTomlStoreError> {
    let parsed: u64 = value.parse().map_err(|_| {
        RelayServeTomlStoreError::new(format!(
            "relay.toml `{key}` must be a whole number of seconds, got {value:?}"
        ))
    })?;
    if parsed == 0 {
        return Err(RelayServeTomlStoreError::new(format!(
            "relay.toml `{key}` must be greater than 0"
        )));
    }
    Ok(parsed)
}

fn parse_positive_usize(key: &str, value: &str) -> Result<usize, RelayServeTomlStoreError> {
    let parsed: usize = value.parse().map_err(|_| {
        RelayServeTomlStoreError::new(format!(
            "relay.toml `{key}` must be a positive whole number, got {value:?}"
        ))
    })?;
    if parsed == 0 {
        return Err(RelayServeTomlStoreError::new(format!(
            "relay.toml `{key}` must be greater than 0"
        )));
    }
    Ok(parsed)
}

fn parse_key_value(line: &str) -> Result<(String, String), RelayServeTomlStoreError> {
    let Some((key, value)) = line.split_once('=') else {
        return Err(RelayServeTomlStoreError::new(format!(
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

fn parse_quoted(value: &str) -> Result<String, RelayServeTomlStoreError> {
    if !value.ends_with('"') || value.len() < 2 {
        return Err(RelayServeTomlStoreError::new(
            "unterminated relay.toml string".to_string(),
        ));
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
            "waitagent-relay-serve-toml-{name}-{}-{}.toml",
            std::process::id(),
            std::thread::current()
                .name()
                .unwrap_or("test")
                .replace(":", "_")
        ))
    }

    fn sample() -> RelayServeTomlConfig {
        RelayServeTomlConfig {
            listen: Some("127.0.0.1:7475".parse().expect("socket addr")),
            admin_socket: Some(PathBuf::from("/tmp/waitagent-relay-admin.sock")),
            whitelist_dir: Some(PathBuf::from("/tmp/authorized_nodes")),
            token_invite_ttl_secs: Some(900),
            token_deploy_ttl_secs: Some(86_400),
            heartbeat_offline_after_secs: Some(45),
            register_timeout_secs: Some(10),
            capacity_max_nodes: Some(8),
            capacity_max_streams: Some(16),
            capacity_max_throughput_bytes_per_sec: Some(1024 * 1024),
        }
    }

    #[test]
    fn default_path_uses_waitagent_home() {
        let path = default_path();
        assert!(path.ends_with(PathBuf::from(".waitagent/relay.toml")));
    }

    #[test]
    fn round_trips_every_key() {
        let path = unique_path("round-trip");
        let config = sample();
        config.save(&path).expect("save should succeed");

        let loaded = RelayServeTomlConfig::load(&path).expect("load should succeed");
        assert_eq!(loaded, Some(config));
        crate::infra::best_effort::remove_file(&path);
    }

    #[test]
    fn load_missing_file_yields_none() {
        let path = unique_path("missing");
        assert_eq!(RelayServeTomlConfig::load(&path).expect("ok"), None);
    }

    #[test]
    fn loads_any_subset_of_keys() {
        let path = unique_path("subset");
        fs::write(
            &path,
            "listen = \"127.0.0.1:9999\"\ncapacity_max_nodes = 4\n",
        )
        .expect("write should succeed");
        let loaded = RelayServeTomlConfig::load(&path).expect("load should succeed");
        let expected = RelayServeTomlConfig {
            listen: Some("127.0.0.1:9999".parse().expect("socket addr")),
            capacity_max_nodes: Some(4),
            ..RelayServeTomlConfig::default()
        };
        assert_eq!(loaded, Some(expected));
        crate::infra::best_effort::remove_file(&path);
    }

    #[test]
    fn rejects_unknown_keys_naming_the_key() {
        let path = unique_path("unknown-key");
        fs::write(&path, "listen = \"127.0.0.1:7475\"\nsurprise = 1\n")
            .expect("write should succeed");
        let error = RelayServeTomlConfig::load(&path).expect_err("unknown key should fail");
        assert!(error
            .to_string()
            .contains("unknown relay.toml field `surprise`"));
        crate::infra::best_effort::remove_file(&path);
    }

    #[test]
    fn rejects_a_non_socket_listen() {
        let path = unique_path("bad-listen");
        fs::write(&path, "listen = \"not-an-address\"\n").expect("write should succeed");
        let error = RelayServeTomlConfig::load(&path).expect_err("bad listen should fail");
        assert!(
            error.to_string().contains("`listen`"),
            "the error names the key: {error}"
        );
        crate::infra::best_effort::remove_file(&path);
    }

    #[test]
    fn rejects_a_relative_admin_socket() {
        let path = unique_path("relative-admin");
        fs::write(&path, "admin_socket = \"relays/admin.sock\"\n").expect("write should succeed");
        let error = RelayServeTomlConfig::load(&path).expect_err("relative path should fail");
        assert!(
            error.to_string().contains("`admin_socket`") && error.to_string().contains("absolute"),
            "the error names the key and the fix: {error}"
        );
        crate::infra::best_effort::remove_file(&path);
    }

    #[test]
    fn rejects_zero_values_for_positive_keys() {
        for (name, line) in [
            ("invite-ttl", "token_invite_ttl_secs = 0"),
            ("deploy-ttl", "token_deploy_ttl_secs = 0"),
            ("offline", "heartbeat_offline_after_secs = 0"),
            ("register-timeout", "register_timeout_secs = 0"),
            ("max-nodes", "capacity_max_nodes = 0"),
            ("max-streams", "capacity_max_streams = 0"),
            ("throughput", "capacity_max_throughput_bytes_per_sec = 0"),
        ] {
            let path = unique_path(name);
            fs::write(&path, format!("{line}\n")).expect("write should succeed");
            let error = RelayServeTomlConfig::load(&path).expect_err("zero should fail");
            assert!(
                error.to_string().contains("must be greater than 0"),
                "{line}: {error}"
            );
            crate::infra::best_effort::remove_file(&path);
        }
    }

    #[test]
    fn rejects_non_numeric_values() {
        let path = unique_path("non-numeric");
        fs::write(&path, "heartbeat_offline_after_secs = soon\n").expect("write should succeed");
        let error = RelayServeTomlConfig::load(&path).expect_err("non-numeric should fail");
        assert!(
            error.to_string().contains("`heartbeat_offline_after_secs`"),
            "the error names the key: {error}"
        );
        crate::infra::best_effort::remove_file(&path);
    }

    #[test]
    fn round_trips_paths_with_special_characters() {
        let path = unique_path("path-escapes");
        let config = RelayServeTomlConfig {
            admin_socket: Some(PathBuf::from("/tmp/ad\"min\\admin\n.sock")),
            ..RelayServeTomlConfig::default()
        };
        config.save(&path).expect("save should succeed");
        let loaded = RelayServeTomlConfig::load(&path).expect("load should succeed");
        assert_eq!(loaded, Some(config));
        crate::infra::best_effort::remove_file(&path);
    }
}
