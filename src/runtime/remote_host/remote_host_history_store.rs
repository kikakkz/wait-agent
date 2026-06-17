#![allow(dead_code)]

use crate::runtime::remote_host::remote_host_home::waitagent_home;
use crate::runtime::remote_host::remote_host_secret_store::RemoteHostSecretId;
use std::fmt;
use std::fs;
use std::io;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteHostProfile {
    pub name: String,
    pub host: String,
    pub ssh_user: String,
    pub auth: RemoteHostAuthProfile,
    pub sudo_password_secret_id: Option<RemoteHostSecretId>,
    pub preferred_remote_port: RemotePortPreference,
    pub last_remote_port: Option<u16>,
    pub last_endpoint: Option<String>,
    pub last_connected_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteHostAuthProfile {
    Password {
        password_secret_id: Option<RemoteHostSecretId>,
    },
    Key {
        key_path: PathBuf,
    },
    Agent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemotePortPreference {
    Auto,
    Port(u16),
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RemoteHostHistory {
    pub hosts: Vec<RemoteHostProfile>,
}

#[derive(Debug, Clone)]
pub struct RemoteHostHistoryStore {
    path: PathBuf,
}

impl RemoteHostHistoryStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn default_path() -> PathBuf {
        waitagent_home().join("remote-hosts.toml")
    }

    pub fn load(&self) -> Result<RemoteHostHistory, RemoteHostHistoryStoreError> {
        if !self.path.exists() {
            return Ok(RemoteHostHistory::default());
        }
        let text = fs::read_to_string(&self.path).map_err(RemoteHostHistoryStoreError::io)?;
        parse_history(&text)
    }

    pub fn save(&self, history: &RemoteHostHistory) -> Result<(), RemoteHostHistoryStoreError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(RemoteHostHistoryStoreError::io)?;
        }
        let content = serialize_history(history);
        fs::write(&self.path, content).map_err(RemoteHostHistoryStoreError::io)
    }

    pub fn upsert_profile(
        &self,
        profile: RemoteHostProfile,
    ) -> Result<(), RemoteHostHistoryStoreError> {
        let mut history = self.load()?;
        if let Some(existing) = history
            .hosts
            .iter_mut()
            .find(|host| host.name == profile.name)
        {
            *existing = profile;
        } else {
            history.hosts.push(profile);
        }
        self.save(&history)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteHostHistoryStoreError {
    message: String,
}

impl RemoteHostHistoryStoreError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    fn io(error: io::Error) -> Self {
        Self::new(error.to_string())
    }
}

impl fmt::Display for RemoteHostHistoryStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for RemoteHostHistoryStoreError {}

fn serialize_history(history: &RemoteHostHistory) -> String {
    let mut out = String::new();
    for host in &history.hosts {
        out.push_str("[[hosts]]\n");
        push_string(&mut out, "name", &host.name);
        push_string(&mut out, "host", &host.host);
        push_string(&mut out, "ssh_user", &host.ssh_user);
        match &host.auth {
            RemoteHostAuthProfile::Password { password_secret_id } => {
                push_string(&mut out, "auth_kind", "password");
                if let Some(secret_id) = password_secret_id {
                    push_string(&mut out, "ssh_password_secret_id", secret_id.as_str());
                }
                push_string(&mut out, "key_path", "");
            }
            RemoteHostAuthProfile::Key { key_path } => {
                push_string(&mut out, "auth_kind", "key");
                push_string(&mut out, "key_path", &key_path.to_string_lossy());
            }
            RemoteHostAuthProfile::Agent => {
                push_string(&mut out, "auth_kind", "agent");
                push_string(&mut out, "key_path", "");
            }
        }
        if let Some(secret_id) = &host.sudo_password_secret_id {
            push_string(&mut out, "sudo_password_secret_id", secret_id.as_str());
        }
        match host.preferred_remote_port {
            RemotePortPreference::Auto => push_string(&mut out, "preferred_remote_port", "auto"),
            RemotePortPreference::Port(port) => {
                out.push_str(&format!("preferred_remote_port = {}\n", port));
            }
        }
        if let Some(port) = host.last_remote_port {
            out.push_str(&format!("last_remote_port = {}\n", port));
        }
        if let Some(endpoint) = &host.last_endpoint {
            push_string(&mut out, "last_endpoint", endpoint);
        }
        if let Some(connected_at) = &host.last_connected_at {
            push_string(&mut out, "last_connected_at", connected_at);
        }
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

fn parse_history(text: &str) -> Result<RemoteHostHistory, RemoteHostHistoryStoreError> {
    let mut hosts = Vec::new();
    let mut current = RawProfile::default();
    let mut in_host = false;

    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line == "[[hosts]]" {
            if in_host {
                hosts.push(current.into_profile()?);
                current = RawProfile::default();
            }
            in_host = true;
            continue;
        }
        if !in_host {
            return Err(RemoteHostHistoryStoreError::new(
                "remote host history field appears before [[hosts]]",
            ));
        }
        let (key, value) = parse_key_value(line)?;
        current.set(&key, value)?;
    }

    if in_host {
        hosts.push(current.into_profile()?);
    }

    Ok(RemoteHostHistory { hosts })
}

#[derive(Default)]
struct RawProfile {
    name: Option<String>,
    host: Option<String>,
    ssh_user: Option<String>,
    auth_kind: Option<String>,
    ssh_password_secret_id: Option<String>,
    sudo_password_secret_id: Option<String>,
    key_path: Option<String>,
    preferred_remote_port: Option<String>,
    last_remote_port: Option<String>,
    last_endpoint: Option<String>,
    last_connected_at: Option<String>,
}

impl RawProfile {
    fn set(&mut self, key: &str, value: String) -> Result<(), RemoteHostHistoryStoreError> {
        match key {
            "name" => self.name = Some(value),
            "host" => self.host = Some(value),
            "ssh_user" => self.ssh_user = Some(value),
            "auth_kind" => self.auth_kind = Some(value),
            "ssh_password_secret_id" => self.ssh_password_secret_id = Some(value),
            "sudo_password_secret_id" => self.sudo_password_secret_id = Some(value),
            "key_path" => self.key_path = Some(value),
            "preferred_remote_port" => self.preferred_remote_port = Some(value),
            "last_remote_port" => self.last_remote_port = Some(value),
            "last_endpoint" => self.last_endpoint = Some(value),
            "last_connected_at" => self.last_connected_at = Some(value),
            other => {
                return Err(RemoteHostHistoryStoreError::new(format!(
                    "unknown remote host history field `{other}`"
                )));
            }
        }
        Ok(())
    }

    fn into_profile(self) -> Result<RemoteHostProfile, RemoteHostHistoryStoreError> {
        let auth_kind = self.auth_kind.unwrap_or_else(|| "agent".to_string());
        let auth = match auth_kind.as_str() {
            "password" => RemoteHostAuthProfile::Password {
                password_secret_id: optional_secret_id(self.ssh_password_secret_id)?,
            },
            "key" => RemoteHostAuthProfile::Key {
                key_path: PathBuf::from(self.key_path.unwrap_or_default()),
            },
            "agent" => RemoteHostAuthProfile::Agent,
            other => {
                return Err(RemoteHostHistoryStoreError::new(format!(
                    "unknown remote host auth kind `{other}`"
                )));
            }
        };
        Ok(RemoteHostProfile {
            name: required(self.name, "name")?,
            host: required(self.host, "host")?,
            ssh_user: required(self.ssh_user, "ssh_user")?,
            auth,
            sudo_password_secret_id: optional_secret_id(self.sudo_password_secret_id)?,
            preferred_remote_port: parse_port_preference(self.preferred_remote_port)?,
            last_remote_port: optional_u16(self.last_remote_port, "last_remote_port")?,
            last_endpoint: self.last_endpoint.filter(|value| !value.is_empty()),
            last_connected_at: self.last_connected_at.filter(|value| !value.is_empty()),
        })
    }
}

fn parse_key_value(line: &str) -> Result<(String, String), RemoteHostHistoryStoreError> {
    let Some((key, value)) = line.split_once('=') else {
        return Err(RemoteHostHistoryStoreError::new(format!(
            "invalid remote host history line `{line}`"
        )));
    };
    let key = key.trim().to_string();
    let value = value.trim();
    if value.starts_with('"') {
        return Ok((key, parse_quoted(value)?));
    }
    Ok((key, value.to_string()))
}

fn parse_quoted(value: &str) -> Result<String, RemoteHostHistoryStoreError> {
    if !value.ends_with('"') || value.len() < 2 {
        return Err(RemoteHostHistoryStoreError::new(
            "unterminated remote host history string",
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

fn required(value: Option<String>, field: &str) -> Result<String, RemoteHostHistoryStoreError> {
    match value.filter(|value| !value.trim().is_empty()) {
        Some(value) => Ok(value),
        None => Err(RemoteHostHistoryStoreError::new(format!(
            "remote host profile `{field}` is required"
        ))),
    }
}

fn optional_secret_id(
    value: Option<String>,
) -> Result<Option<RemoteHostSecretId>, RemoteHostHistoryStoreError> {
    value
        .filter(|value| !value.trim().is_empty())
        .map(RemoteHostSecretId::new)
        .transpose()
        .map_err(|error| RemoteHostHistoryStoreError::new(error.to_string()))
}

fn parse_port_preference(
    value: Option<String>,
) -> Result<RemotePortPreference, RemoteHostHistoryStoreError> {
    let Some(value) = value else {
        return Ok(RemotePortPreference::Auto);
    };
    if value == "auto" {
        return Ok(RemotePortPreference::Auto);
    }
    Ok(RemotePortPreference::Port(parse_u16(
        &value,
        "preferred_remote_port",
    )?))
}

fn optional_u16(
    value: Option<String>,
    field: &str,
) -> Result<Option<u16>, RemoteHostHistoryStoreError> {
    value
        .filter(|value| !value.trim().is_empty())
        .map(|value| parse_u16(&value, field))
        .transpose()
}

fn parse_u16(value: &str, field: &str) -> Result<u16, RemoteHostHistoryStoreError> {
    value.parse::<u16>().map_err(|_| {
        RemoteHostHistoryStoreError::new(format!("remote host profile `{field}` must be a port"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::remote_host::remote_host_secret_store::{
        MemoryRemoteHostSecretStore, RemoteHostSecretStore, RemoteHostSecretValue,
    };

    #[test]
    fn remote_host_history_default_path_uses_user_config_dir() {
        let path = RemoteHostHistoryStore::default_path();
        assert!(path.ends_with(PathBuf::from(".waitagent/remote-hosts.toml")));
    }

    #[test]
    fn remote_host_history_persists_secret_references_without_password_values() {
        let path = unique_path("remote-hosts-secret-refs.toml");
        let store = RemoteHostHistoryStore::new(&path);
        let secret_store = MemoryRemoteHostSecretStore::default();
        let ssh_secret_id =
            RemoteHostSecretId::new("waitagent.remote-host.130.ssh-password").unwrap();
        let sudo_secret_id =
            RemoteHostSecretId::new("waitagent.remote-host.130.sudo-password").unwrap();

        secret_store
            .put_secret(&ssh_secret_id, RemoteHostSecretValue::new("12345679"))
            .unwrap();
        secret_store
            .put_secret(&sudo_secret_id, RemoteHostSecretValue::new("sudo-secret"))
            .unwrap();

        store
            .upsert_profile(RemoteHostProfile {
                name: "130".to_string(),
                host: "10.1.29.130".to_string(),
                ssh_user: "kk".to_string(),
                auth: RemoteHostAuthProfile::Password {
                    password_secret_id: Some(ssh_secret_id.clone()),
                },
                sudo_password_secret_id: Some(sudo_secret_id.clone()),
                preferred_remote_port: RemotePortPreference::Auto,
                last_remote_port: Some(7476),
                last_endpoint: Some("10.1.29.130:7476".to_string()),
                last_connected_at: Some("2026-06-16T00:00:00Z".to_string()),
            })
            .unwrap();

        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("ssh_password_secret_id"));
        assert!(content.contains("sudo_password_secret_id"));
        assert!(!content.contains("12345679"));
        assert!(!content.contains("sudo-secret"));

        let loaded = store.load().unwrap();
        assert_eq!(loaded.hosts.len(), 1);
        assert_eq!(loaded.hosts[0].name, "130");
        assert_eq!(
            loaded.hosts[0].auth,
            RemoteHostAuthProfile::Password {
                password_secret_id: Some(ssh_secret_id.clone())
            }
        );
        assert_eq!(
            secret_store
                .get_secret(&ssh_secret_id)
                .unwrap()
                .unwrap()
                .expose_secret(),
            "12345679"
        );

        let _ = fs::remove_file(path);
    }

    #[test]
    fn remote_host_history_updates_existing_profile_by_name() {
        let path = unique_path("remote-hosts-upsert.toml");
        let store = RemoteHostHistoryStore::new(&path);

        store.upsert_profile(profile("130", "10.1.29.130")).unwrap();
        store.upsert_profile(profile("130", "10.1.29.131")).unwrap();

        let loaded = store.load().unwrap();
        assert_eq!(loaded.hosts.len(), 1);
        assert_eq!(loaded.hosts[0].host, "10.1.29.131");

        let _ = fs::remove_file(path);
    }

    fn profile(name: &str, host: &str) -> RemoteHostProfile {
        RemoteHostProfile {
            name: name.to_string(),
            host: host.to_string(),
            ssh_user: "kk".to_string(),
            auth: RemoteHostAuthProfile::Agent,
            sudo_password_secret_id: None,
            preferred_remote_port: RemotePortPreference::Port(7474),
            last_remote_port: None,
            last_endpoint: None,
            last_connected_at: None,
        }
    }

    fn unique_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "waitagent-{name}-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ))
    }
}
