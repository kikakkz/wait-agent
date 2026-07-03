#![allow(dead_code)]

use crate::runtime::remote_host::remote_host_home::waitagent_home;
use std::fmt;
use std::fs;
use std::io;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RemoteInstallProxyConfig {
    pub all_proxy: String,
    pub https_proxy: String,
}

impl RemoteInstallProxyConfig {
    pub fn has_proxy(&self) -> bool {
        !self.all_proxy.trim().is_empty() || !self.https_proxy.trim().is_empty()
    }

    pub fn validate(&self) -> Result<(), RemoteInstallProxyStoreError> {
        validate_proxy_url("all_proxy", &self.all_proxy)?;
        validate_proxy_url("https_proxy", &self.https_proxy)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteInstallProxyProfile {
    pub name: String,
    pub all_proxy: String,
    pub https_proxy: String,
}

impl RemoteInstallProxyProfile {
    pub fn config(&self) -> RemoteInstallProxyConfig {
        RemoteInstallProxyConfig {
            all_proxy: self.all_proxy.clone(),
            https_proxy: self.https_proxy.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RemoteInstallProxySettings {
    pub active: Option<String>,
    pub profiles: Vec<RemoteInstallProxyProfile>,
}

impl RemoteInstallProxySettings {
    pub fn active_config(&self) -> RemoteInstallProxyConfig {
        let Some(active) = &self.active else {
            return RemoteInstallProxyConfig::default();
        };
        self.profiles
            .iter()
            .find(|profile| &profile.name == active)
            .map(RemoteInstallProxyProfile::config)
            .unwrap_or_default()
    }

    pub fn validate(&self) -> Result<(), RemoteInstallProxyStoreError> {
        let mut names = Vec::new();
        for profile in &self.profiles {
            let name = profile.name.trim();
            if name.is_empty() {
                return Err(RemoteInstallProxyStoreError::new(
                    "proxy profile name is required",
                ));
            }
            if names.iter().any(|existing| existing == name) {
                return Err(RemoteInstallProxyStoreError::new(format!(
                    "duplicate proxy profile `{name}`"
                )));
            }
            names.push(name.to_string());
            profile.config().validate()?;
        }
        if let Some(active) = &self.active {
            if !active.trim().is_empty()
                && !self.profiles.iter().any(|profile| profile.name == *active)
            {
                return Err(RemoteInstallProxyStoreError::new(format!(
                    "active proxy profile `{active}` does not exist"
                )));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct RemoteInstallProxyStore {
    path: PathBuf,
}

impl RemoteInstallProxyStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn default_path() -> PathBuf {
        waitagent_home().join("remote-install-proxy.toml")
    }

    pub fn load_settings(
        &self,
    ) -> Result<RemoteInstallProxySettings, RemoteInstallProxyStoreError> {
        if !self.path.exists() {
            return Ok(RemoteInstallProxySettings::default());
        }
        parse_settings(&fs::read_to_string(&self.path).map_err(RemoteInstallProxyStoreError::io)?)
    }

    pub fn load_active_config(
        &self,
    ) -> Result<RemoteInstallProxyConfig, RemoteInstallProxyStoreError> {
        self.load_settings()
            .map(|settings| settings.active_config())
    }

    pub fn save_settings(
        &self,
        settings: &RemoteInstallProxySettings,
    ) -> Result<(), RemoteInstallProxyStoreError> {
        settings.validate()?;
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(RemoteInstallProxyStoreError::io)?;
        }
        fs::write(&self.path, serialize_settings(settings))
            .map_err(RemoteInstallProxyStoreError::io)
    }
}

impl Default for RemoteInstallProxyStore {
    fn default() -> Self {
        Self::new(Self::default_path())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteInstallProxyStoreError {
    message: String,
}

impl RemoteInstallProxyStoreError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    fn io(error: io::Error) -> Self {
        Self::new(error.to_string())
    }
}

impl fmt::Display for RemoteInstallProxyStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for RemoteInstallProxyStoreError {}

pub fn no_proxy_for_install(remote_host: &str, local_connect_endpoint: &str) -> String {
    let mut entries = vec![
        "localhost".to_string(),
        "127.0.0.1".to_string(),
        "::1".to_string(),
        "10.0.0.0/8".to_string(),
        "172.16.0.0/12".to_string(),
        "192.168.0.0/16".to_string(),
        "169.254.0.0/16".to_string(),
        "fc00::/7".to_string(),
        "fe80::/10".to_string(),
    ];
    push_unique(&mut entries, endpoint_host(remote_host));
    push_unique(&mut entries, endpoint_host(local_connect_endpoint));
    entries.join(",")
}

pub fn proxy_env_prefix(
    config: &RemoteInstallProxyConfig,
    remote_host: &str,
    local_connect_endpoint: &str,
) -> String {
    let no_proxy = no_proxy_for_install(remote_host, local_connect_endpoint);
    let mut parts = Vec::new();
    if !config.all_proxy.trim().is_empty() {
        parts.push(format!(
            "all_proxy={}",
            shell_single_quote(config.all_proxy.trim())
        ));
        parts.push(format!(
            "ALL_PROXY={}",
            shell_single_quote(config.all_proxy.trim())
        ));
    }
    if !config.https_proxy.trim().is_empty() {
        parts.push(format!(
            "https_proxy={}",
            shell_single_quote(config.https_proxy.trim())
        ));
        parts.push(format!(
            "HTTPS_PROXY={}",
            shell_single_quote(config.https_proxy.trim())
        ));
    }
    parts.push(format!("no_proxy={}", shell_single_quote(&no_proxy)));
    parts.push(format!("NO_PROXY={}", shell_single_quote(&no_proxy)));
    parts.join(" ")
}

fn proxy_export_prefix(
    config: &RemoteInstallProxyConfig,
    remote_host: &str,
    local_connect_endpoint: &str,
) -> String {
    proxy_env_prefix(config, remote_host, local_connect_endpoint)
        .split_whitespace()
        .map(|assignment| format!("export {assignment};"))
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn wrap_install_command_with_proxy(
    command: &str,
    config: &RemoteInstallProxyConfig,
    remote_host: &str,
    local_connect_endpoint: &str,
) -> Result<String, RemoteInstallProxyStoreError> {
    if !config.has_proxy() {
        return Ok(command.to_string());
    }
    config.validate()?;
    Ok(format!(
        "{} sh -lc {}",
        proxy_export_prefix(config, remote_host, local_connect_endpoint),
        shell_single_quote(command)
    ))
}

fn validate_proxy_url(field: &str, value: &str) -> Result<(), RemoteInstallProxyStoreError> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(());
    }
    if value.contains('：') {
        return Err(RemoteInstallProxyStoreError::new(format!(
            "{field} contains a full-width colon; use `:`"
        )));
    }
    let Some((scheme, rest)) = value.split_once("://") else {
        return Err(RemoteInstallProxyStoreError::new(format!(
            "{field} must include a URL scheme"
        )));
    };
    if scheme.is_empty() || !scheme.chars().all(|ch| ch.is_ascii_alphanumeric()) {
        return Err(RemoteInstallProxyStoreError::new(format!(
            "{field} has an invalid URL scheme"
        )));
    }
    if rest.is_empty() || rest.starts_with(':') {
        return Err(RemoteInstallProxyStoreError::new(format!(
            "{field} must include a host"
        )));
    }
    if let Some(port) = rest.rsplit_once(':').map(|(_, port)| port) {
        let port = port.split('/').next().unwrap_or(port);
        if port.is_empty() || !port.chars().all(|ch| ch.is_ascii_digit()) {
            return Err(RemoteInstallProxyStoreError::new(format!(
                "{field} has an invalid port"
            )));
        }
    }
    Ok(())
}

fn parse_settings(text: &str) -> Result<RemoteInstallProxySettings, RemoteInstallProxyStoreError> {
    let mut settings = RemoteInstallProxySettings::default();
    let mut current: Option<RemoteInstallProxyProfile> = None;
    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line == "[[proxy]]" {
            if let Some(profile) = current.take() {
                settings.profiles.push(profile);
            }
            current = Some(RemoteInstallProxyProfile {
                name: String::new(),
                all_proxy: String::new(),
                https_proxy: String::new(),
            });
            continue;
        }
        let (key, value) = parse_key_value(line)?;
        if let Some(profile) = &mut current {
            match key.as_str() {
                "name" => profile.name = value,
                "all_proxy" => profile.all_proxy = value,
                "https_proxy" => profile.https_proxy = value,
                other => {
                    return Err(RemoteInstallProxyStoreError::new(format!(
                        "unknown remote install proxy field `{other}`"
                    )));
                }
            }
        } else {
            match key.as_str() {
                "active" => {
                    settings.active = if value.trim().is_empty() {
                        None
                    } else {
                        Some(value)
                    };
                }
                other => {
                    return Err(RemoteInstallProxyStoreError::new(format!(
                        "unknown remote install proxy field `{other}`"
                    )));
                }
            }
        }
    }
    if let Some(profile) = current {
        settings.profiles.push(profile);
    }
    settings.validate()?;
    Ok(settings)
}

fn serialize_settings(settings: &RemoteInstallProxySettings) -> String {
    let mut out = String::new();
    push_string(&mut out, "active", settings.active.as_deref().unwrap_or(""));
    for profile in &settings.profiles {
        out.push('\n');
        out.push_str("[[proxy]]\n");
        push_string(&mut out, "name", &profile.name);
        push_string(&mut out, "all_proxy", &profile.all_proxy);
        push_string(&mut out, "https_proxy", &profile.https_proxy);
    }
    out
}

fn parse_key_value(line: &str) -> Result<(String, String), RemoteInstallProxyStoreError> {
    let Some((key, value)) = line.split_once('=') else {
        return Err(RemoteInstallProxyStoreError::new(format!(
            "invalid remote install proxy line `{line}`"
        )));
    };
    let key = key.trim().to_string();
    let value = value.trim();
    if value.starts_with('"') {
        return Ok((key, parse_quoted(value)?));
    }
    Ok((key, value.to_string()))
}

fn parse_quoted(value: &str) -> Result<String, RemoteInstallProxyStoreError> {
    if !value.ends_with('"') || value.len() < 2 {
        return Err(RemoteInstallProxyStoreError::new(
            "unterminated remote install proxy string",
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

fn push_unique(entries: &mut Vec<String>, value: Option<String>) {
    let Some(value) = value else {
        return;
    };
    if value.trim().is_empty() || entries.iter().any(|entry| entry == &value) {
        return;
    }
    entries.push(value);
}

fn endpoint_host(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let value = value
        .strip_prefix("http://")
        .or_else(|| value.strip_prefix("https://"))
        .unwrap_or(value);
    if let Some(stripped) = value.strip_prefix('[') {
        return stripped.split_once(']').map(|(host, _)| host.to_string());
    }
    let host = value.split('/').next().unwrap_or(value);
    Some(host.split(':').next().unwrap_or(host).to_string())
}

fn shell_single_quote(value: &str) -> String {
    let quote = char::from(39);
    let slash = char::from(92);
    let mut out = String::new();
    out.push(quote);
    for ch in value.chars() {
        if ch == quote {
            out.push(quote);
            out.push(slash);
            out.push(quote);
            out.push(quote);
        } else {
            out.push(ch);
        }
    }
    out.push(quote);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn proxy_store_round_trips_settings_and_active_config() {
        let path = unique_path("remote-install-proxy.toml");
        let store = RemoteInstallProxyStore::new(&path);
        let settings = RemoteInstallProxySettings {
            active: Some("office".to_string()),
            profiles: vec![
                RemoteInstallProxyProfile {
                    name: "home".to_string(),
                    all_proxy: "socks5://192.168.31.1:7897".to_string(),
                    https_proxy: "http://192.168.31.1:7897".to_string(),
                },
                RemoteInstallProxyProfile {
                    name: "office".to_string(),
                    all_proxy: "socks5://127.0.0.1:7897".to_string(),
                    https_proxy: "http://127.0.0.1:7897".to_string(),
                },
            ],
        };
        store.save_settings(&settings).unwrap();
        assert_eq!(store.load_settings().unwrap(), settings);
        assert_eq!(
            store.load_active_config().unwrap(),
            RemoteInstallProxyConfig {
                all_proxy: "socks5://127.0.0.1:7897".to_string(),
                https_proxy: "http://127.0.0.1:7897".to_string(),
            }
        );
        let _ = fs::remove_file(path);
    }

    #[test]
    fn proxy_store_rejects_duplicate_names() {
        let settings = RemoteInstallProxySettings {
            active: Some("office".to_string()),
            profiles: vec![
                RemoteInstallProxyProfile {
                    name: "office".to_string(),
                    all_proxy: String::new(),
                    https_proxy: String::new(),
                },
                RemoteInstallProxyProfile {
                    name: "office".to_string(),
                    all_proxy: String::new(),
                    https_proxy: String::new(),
                },
            ],
        };

        let error = settings.validate().unwrap_err();

        assert!(error.to_string().contains("duplicate proxy profile"));
    }

    #[test]
    fn no_proxy_includes_lan_and_current_endpoints() {
        let value = no_proxy_for_install("10.1.29.130", "192.168.1.5:7474");
        assert!(value.contains("localhost"));
        assert!(value.contains("10.0.0.0/8"));
        assert!(value.contains("192.168.0.0/16"));
        assert!(value.contains("10.1.29.130"));
        assert!(value.contains("192.168.1.5"));
    }

    #[test]
    fn proxy_wrapper_sets_uppercase_and_lowercase_vars() {
        let config = RemoteInstallProxyConfig {
            all_proxy: "socks5://127.0.0.1:7897".to_string(),
            https_proxy: "http://127.0.0.1:7897".to_string(),
        };
        let command = wrap_install_command_with_proxy(
            "curl -fsSL example | bash",
            &config,
            "10.0.0.2",
            "192.168.1.5:7474",
        )
        .unwrap();
        assert!(command.contains("export all_proxy="));
        assert!(command.contains("export HTTPS_PROXY="));
        assert!(command.contains("export no_proxy="));
        assert!(command.contains("all_proxy="));
        assert!(command.contains("ALL_PROXY="));
        assert!(command.contains("https_proxy="));
        assert!(command.contains("HTTPS_PROXY="));
        assert!(command.contains("no_proxy="));
        assert!(command.contains("NO_PROXY="));
        assert!(command.contains("sh -lc"));
    }

    #[test]
    fn proxy_config_rejects_full_width_colon() {
        let config = RemoteInstallProxyConfig {
            all_proxy: "socks5://127.0.0.1:7897".to_string(),
            https_proxy: "http://127.0.0.1：7897".to_string(),
        };

        let error = config.validate().unwrap_err();

        assert!(error.to_string().contains("full-width colon"));
    }

    fn unique_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("waitagent-{nanos}-{name}"))
    }
}
