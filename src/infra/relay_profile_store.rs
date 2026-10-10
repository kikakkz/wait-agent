//! Saved relay profiles for the Ctrl-W popup: `~/.waitagent/relays.toml`
//! (issue #167). Unlike `relay_toml_store` — the node's single active pin,
//! written by `relay join` — this store keeps every relay the operator has
//! joined (address as the unique key, optional fingerprint, optional display
//! name) so the popup can list them and mark the pinned one with ★.
//!
//! The format is the same hand-rolled TOML subset as the other popup
//! stores: `[[relay]]` tables with `key = "value"` lines, `#` comments, and
//! unknown keys rejected so a stray edit surfaces instead of being silently
//! ignored.

use std::fmt;
use std::fs;
use std::io;
use std::path::PathBuf;

use crate::host::ssh::remote_host_home::waitagent_home;
use crate::infra::relay_join::parse_relay_address;

/// One saved relay: `address` is the unique key, `fingerprint` is the TLS
/// identity the node reported on join (absent until then), and `name` is an
/// optional operator-chosen display label (the sidebar falls back to the
/// address).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RelayProfile {
    /// Relay address with explicit port (`host:port`).
    pub address: String,
    /// Lowercase hex SHA-256 SPKI fingerprint of the relay certificate.
    pub fingerprint: Option<String>,
    /// Optional display name for the sidebar list.
    pub name: Option<String>,
}

impl RelayProfile {
    /// Sidebar label: the display name when set, otherwise the address.
    pub fn label(&self) -> &str {
        self.name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .unwrap_or(self.address.as_str())
    }
}

/// Plain TOML store for [`RelayProfile`]s at `~/.waitagent/relays.toml`.
#[derive(Debug, Clone)]
pub struct RelayProfileStore {
    path: PathBuf,
}

impl RelayProfileStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Returns the default path: `waitagent_home()/relays.toml`.
    pub fn default_path() -> PathBuf {
        waitagent_home().join("relays.toml")
    }

    /// Loads the saved profiles; a missing file yields an empty list.
    pub fn load_profiles(&self) -> Result<Vec<RelayProfile>, RelayProfileStoreError> {
        if !self.path.is_file() {
            return Ok(Vec::new());
        }
        parse_profiles(&fs::read_to_string(&self.path).map_err(RelayProfileStoreError::io)?)
    }

    /// Replaces the whole profile list after validating it (duplicate
    /// addresses are rejected so the address stays a unique key).
    pub fn save_profiles(&self, profiles: &[RelayProfile]) -> Result<(), RelayProfileStoreError> {
        validate_profiles(profiles)?;
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(RelayProfileStoreError::io)?;
        }
        fs::write(&self.path, serialize_profiles(profiles)).map_err(RelayProfileStoreError::io)
    }

    /// Inserts or replaces the profile with the same address; the join flow
    /// calls this so a successful join auto-saves the profile (issue #167).
    pub fn upsert(&self, profile: &RelayProfile) -> Result<(), RelayProfileStoreError> {
        let mut profiles = self.load_profiles()?;
        match profiles
            .iter_mut()
            .find(|existing| existing.address == profile.address)
        {
            Some(existing) => *existing = profile.clone(),
            None => profiles.push(profile.clone()),
        }
        self.save_profiles(&profiles)
    }
}

impl Default for RelayProfileStore {
    fn default() -> Self {
        Self::new(Self::default_path())
    }
}

/// Errors of the relays.toml store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayProfileStoreError {
    message: String,
}

impl RelayProfileStoreError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    fn io(error: io::Error) -> Self {
        Self::new(error.to_string())
    }
}

impl fmt::Display for RelayProfileStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for RelayProfileStoreError {}

fn validate_profiles(profiles: &[RelayProfile]) -> Result<(), RelayProfileStoreError> {
    let mut addresses: Vec<&str> = Vec::with_capacity(profiles.len());
    for profile in profiles {
        let address = profile.address.trim();
        if address.is_empty() {
            return Err(RelayProfileStoreError::new(
                "relay profile address is required",
            ));
        }
        parse_relay_address(address).map_err(|error| {
            RelayProfileStoreError::new(format!(
                "relay profile address `{address}` is not a valid host[:port] ({error})"
            ))
        })?;
        if addresses.contains(&address) {
            return Err(RelayProfileStoreError::new(format!(
                "duplicate relay profile `{address}`"
            )));
        }
        addresses.push(address);
        if let Some(fingerprint) = &profile.fingerprint {
            normalize_fingerprint(fingerprint)?;
        }
    }
    Ok(())
}

/// Fingerprints compare case-insensitively across the relay protocol; the
/// store normalizes to lowercase so consumers get one shape.
fn normalize_fingerprint(fingerprint: &str) -> Result<String, RelayProfileStoreError> {
    if fingerprint.len() != 64 || !fingerprint.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return Err(RelayProfileStoreError::new(
            "relay profile `fingerprint` must be 64 hex characters (the relay certificate fingerprint)".to_string(),
        ));
    }
    Ok(fingerprint.to_lowercase())
}

fn serialize_profiles(profiles: &[RelayProfile]) -> String {
    let mut out = String::new();
    out.push_str("# WaitAgent saved relay profiles (written by the TUI relay page)\n");
    for profile in profiles {
        out.push_str("\n[[relay]]\n");
        push_string(&mut out, "address", &profile.address);
        if let Some(fingerprint) = &profile.fingerprint {
            push_string(&mut out, "fingerprint", fingerprint);
        }
        if let Some(name) = &profile.name {
            if !name.trim().is_empty() {
                push_string(&mut out, "name", name);
            }
        }
    }
    out
}

fn parse_profiles(text: &str) -> Result<Vec<RelayProfile>, RelayProfileStoreError> {
    let mut profiles = Vec::new();
    let mut current: Option<RelayProfile> = None;
    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line == "[[relay]]" {
            if let Some(profile) = current.take() {
                profiles.push(profile);
            }
            current = Some(RelayProfile::default());
            continue;
        }
        let (key, value) = parse_key_value(line)?;
        let Some(profile) = &mut current else {
            return Err(RelayProfileStoreError::new(format!(
                "relays.toml field `{key}` outside a [[relay]] table"
            )));
        };
        match key.as_str() {
            "address" => profile.address = value,
            "fingerprint" => {
                profile.fingerprint = if value.trim().is_empty() {
                    None
                } else {
                    Some(normalize_fingerprint(&value)?)
                };
            }
            "name" => {
                profile.name = if value.trim().is_empty() {
                    None
                } else {
                    Some(value)
                };
            }
            other => {
                return Err(RelayProfileStoreError::new(format!(
                    "unknown relays.toml field `{other}`"
                )));
            }
        }
    }
    if let Some(profile) = current {
        profiles.push(profile);
    }
    validate_profiles(&profiles)?;
    Ok(profiles)
}

fn parse_key_value(line: &str) -> Result<(String, String), RelayProfileStoreError> {
    let Some((key, value)) = line.split_once('=') else {
        return Err(RelayProfileStoreError::new(format!(
            "invalid relays.toml line `{line}`"
        )));
    };
    let key = key.trim().to_string();
    let value = value.trim();
    if value.starts_with('"') {
        return Ok((key, parse_quoted(value)?));
    }
    Ok((key, value.to_string()))
}

fn parse_quoted(value: &str) -> Result<String, RelayProfileStoreError> {
    if !value.ends_with('"') || value.len() < 2 {
        return Err(RelayProfileStoreError::new(
            "unterminated relays.toml string",
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

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "waitagent-relay-profiles-{name}-{}-{}.toml",
            std::process::id(),
            std::thread::current()
                .name()
                .unwrap_or("test")
                .replace(":", "_")
        ))
    }

    fn sample() -> RelayProfile {
        RelayProfile {
            address: "relay.example:7475".to_string(),
            fingerprint: Some(
                "7c857a105486f46defdfc06ffc2dbc54531a4cdb25515488565aae15264543e4".to_string(),
            ),
            name: Some("office".to_string()),
        }
    }

    #[test]
    fn default_path_uses_waitagent_home() {
        let path = RelayProfileStore::default_path();
        assert!(path.ends_with(PathBuf::from(".waitagent/relays.toml")));
    }

    #[test]
    fn load_missing_file_yields_empty_profiles() {
        let store = RelayProfileStore::new(unique_path("missing"));
        assert_eq!(store.load_profiles().expect("ok"), Vec::new());
    }

    #[test]
    fn round_trips_profiles() {
        let path = unique_path("round-trip");
        let store = RelayProfileStore::new(&path);
        let profiles = vec![
            sample(),
            RelayProfile {
                address: "relay-b.example:7475".to_string(),
                fingerprint: None,
                name: None,
            },
        ];
        store.save_profiles(&profiles).expect("save should succeed");

        let loaded = store.load_profiles().expect("load should succeed");
        assert_eq!(loaded, profiles);
        crate::infra::best_effort::remove_file(&path);
    }

    #[test]
    fn upsert_replaces_same_address_and_keeps_order() {
        let path = unique_path("upsert");
        let store = RelayProfileStore::new(&path);
        store
            .save_profiles(&[
                sample(),
                RelayProfile {
                    address: "relay-b.example:7475".to_string(),
                    fingerprint: None,
                    name: None,
                },
            ])
            .expect("save should succeed");

        let updated = RelayProfile {
            fingerprint: Some("ab".repeat(32)),
            name: None,
            ..sample()
        };
        store.upsert(&updated).expect("upsert should succeed");

        let loaded = store.load_profiles().expect("load should succeed");
        assert_eq!(loaded, vec![updated.clone(), loaded[1].clone()]);
        assert_eq!(loaded[0].address, updated.address);
        assert_eq!(loaded[0].fingerprint, updated.fingerprint);
        assert_eq!(
            loaded[0].name, updated.name,
            "name is overwritten on upsert"
        );
        crate::infra::best_effort::remove_file(&path);
    }

    #[test]
    fn upsert_appends_a_new_profile() {
        let path = unique_path("upsert-new");
        let store = RelayProfileStore::new(&path);
        store.upsert(&sample()).expect("upsert should succeed");
        store
            .upsert(&RelayProfile {
                address: "relay-b.example:7475".to_string(),
                fingerprint: None,
                name: None,
            })
            .expect("upsert should succeed");
        let loaded = store.load_profiles().expect("load should succeed");
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[1].address, "relay-b.example:7475");
        crate::infra::best_effort::remove_file(&path);
    }

    #[test]
    fn rejects_duplicate_addresses() {
        let store = RelayProfileStore::new(unique_path("duplicates"));
        let error = store
            .save_profiles(&[sample(), sample()])
            .expect_err("duplicate addresses should fail");
        assert!(error.to_string().contains("duplicate relay profile"));
    }

    #[test]
    fn rejects_an_unparseable_address() {
        let store = RelayProfileStore::new(unique_path("bad-address"));
        let error = store
            .save_profiles(&[RelayProfile {
                address: "host:notaport".to_string(),
                ..sample()
            }])
            .expect_err("bad address should fail");
        assert!(
            error.to_string().contains("`host:notaport`"),
            "the error names the address: {error}"
        );
    }

    #[test]
    fn rejects_a_short_fingerprint() {
        let store = RelayProfileStore::new(unique_path("short-fingerprint"));
        let error = store
            .save_profiles(&[RelayProfile {
                fingerprint: Some("abcd".to_string()),
                ..sample()
            }])
            .expect_err("short fingerprint should fail");
        assert!(error.to_string().contains("64 hex"));
    }

    #[test]
    fn normalizes_an_uppercase_fingerprint() {
        let path = unique_path("uppercase-fingerprint");
        let store = RelayProfileStore::new(&path);
        let uppercased = "7C857A105486F46DEFDFC06FFC2DBC54531A4CDB25515488565AAE15264543E4";
        store
            .save_profiles(&[RelayProfile {
                fingerprint: Some(uppercased.to_string()),
                ..sample()
            }])
            .expect("save should succeed");
        let loaded = store.load_profiles().expect("load should succeed");
        assert_eq!(loaded[0].fingerprint, Some(uppercased.to_lowercase()));
        crate::infra::best_effort::remove_file(&path);
    }

    #[test]
    fn rejects_unknown_keys() {
        let path = unique_path("unknown-key");
        fs::write(
            &path,
            "[[relay]]\naddress = \"relay.example:7475\"\nsurprise = 1\n",
        )
        .expect("write should succeed");
        let store = RelayProfileStore::new(&path);
        let error = store.load_profiles().expect_err("unknown key should fail");
        assert!(error.to_string().contains("unknown relays.toml field"));
        crate::infra::best_effort::remove_file(&path);
    }

    #[test]
    fn rejects_fields_outside_a_relay_table() {
        let path = unique_path("outside-table");
        fs::write(&path, "address = \"relay.example:7475\"\n").expect("write should succeed");
        let store = RelayProfileStore::new(&path);
        let error = store
            .load_profiles()
            .expect_err("fields outside [[relay]] should fail");
        assert!(error.to_string().contains("outside a [[relay]] table"));
        crate::infra::best_effort::remove_file(&path);
    }

    #[test]
    fn label_falls_back_to_the_address() {
        let mut profile = sample();
        assert_eq!(profile.label(), "office");
        profile.name = None;
        assert_eq!(profile.label(), "relay.example:7475");
        profile.name = Some("   ".to_string());
        assert_eq!(profile.label(), "relay.example:7475");
    }
}
