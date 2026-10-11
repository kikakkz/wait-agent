//! Local download proxy config: `~/.waitagent/local-download-proxy.toml`
//! (issue #172). When a saved host profile uses `install_source = "upload"`,
//! the release artifact is downloaded on THIS machine by a curl subprocess
//! (`local_artifact_cache::CurlArtifactFetcher`); the fetcher injects the
//! proxy URLs from this store as `all_proxy` / `https_proxy` environment
//! variables, which curl honors natively (alongside any `no_proxy` the
//! operator's environment already exports).
//!
//! The format is a hand-rolled two-key TOML-line subset, same idiom as
//! `relay_toml_store`: `key = "value"` lines, `#` comments, unknown keys
//! rejected so a stray edit surfaces instead of being silently ignored.
//! A missing file means a direct connection (no proxy).

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::host::ssh::remote_host_home::waitagent_home;

/// Optional proxy URLs for the local artifact download (issue #172). Both
/// keys are optional; when neither is set the download connects directly.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LocalDownloadProxyConfig {
    /// `all_proxy` URL (e.g. `socks5://127.0.0.1:7897`); curl applies it to
    /// the artifact download. `None` (or empty) leaves it unset.
    pub all_proxy: Option<String>,
    /// `https_proxy` URL (e.g. `http://127.0.0.1:7890`); curl applies it to
    /// the HTTPS artifact download. `None` (or empty) leaves it unset.
    pub https_proxy: Option<String>,
}

impl LocalDownloadProxyConfig {
    /// Returns the default path: `waitagent_home()/local-download-proxy.toml`.
    pub fn default_path() -> PathBuf {
        waitagent_home().join("local-download-proxy.toml")
    }

    /// Writes the config to `path`, creating the parent directory when
    /// missing.
    // TODO(cleanup): no writer UI yet (issue #172 ships read-side only);
    // kept as the store's save counterpart, exercised by the unit tests.
    #[allow(dead_code)]
    pub fn save(&self, path: &Path) -> Result<(), LocalDownloadProxyStoreError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(LocalDownloadProxyStoreError::io)?;
        }
        fs::write(path, serialize_proxy_toml(self)).map_err(LocalDownloadProxyStoreError::io)
    }

    /// Loads the config from `path`; a missing file yields `None` (direct
    /// connection). A present but malformed file is an error: the operator
    /// must fix the config rather than have a typo silently bypass their
    /// proxy (same policy as `remote_install_proxy_store`).
    pub fn load(path: &Path) -> Result<Option<Self>, LocalDownloadProxyStoreError> {
        if !path.is_file() {
            return Ok(None);
        }
        let text = fs::read_to_string(path).map_err(LocalDownloadProxyStoreError::io)?;
        parse_proxy_toml(&text).map(Some)
    }

    /// True when at least one proxy URL is configured (non-empty).
    // TODO(cleanup): read-side only for now; a proxy settings UI lands with
    // the store's writer.
    #[allow(dead_code)]
    pub fn has_proxy(&self) -> bool {
        non_empty(self.all_proxy.as_deref()).is_some()
            || non_empty(self.https_proxy.as_deref()).is_some()
    }

    /// The `all_proxy`/`https_proxy` environment assignments for the curl
    /// subprocess. Only configured (non-empty) URLs are returned, so the
    /// command inherits everything else — including any operator-exported
    /// `no_proxy` — from the node process environment.
    pub fn env_assignments(&self) -> Vec<(&'static str, String)> {
        let mut assignments = Vec::with_capacity(2);
        if let Some(value) = non_empty(self.all_proxy.as_deref()) {
            assignments.push(("all_proxy", value.to_string()));
        }
        if let Some(value) = non_empty(self.https_proxy.as_deref()) {
            assignments.push(("https_proxy", value.to_string()));
        }
        assignments
    }
}

/// Errors of the local-download-proxy store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalDownloadProxyStoreError {
    message: String,
}

impl LocalDownloadProxyStoreError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    fn io(error: io::Error) -> Self {
        Self::new(error.to_string())
    }
}

impl fmt::Display for LocalDownloadProxyStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for LocalDownloadProxyStoreError {}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn serialize_proxy_toml(config: &LocalDownloadProxyConfig) -> String {
    let mut out = String::new();
    out.push_str(
        "# WaitAgent local download proxy (used when a host profile installs via Upload)\n",
    );
    if let Some(value) = non_empty(config.all_proxy.as_deref()) {
        push_string(&mut out, "all_proxy", value);
    }
    if let Some(value) = non_empty(config.https_proxy.as_deref()) {
        push_string(&mut out, "https_proxy", value);
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

fn parse_proxy_toml(text: &str) -> Result<LocalDownloadProxyConfig, LocalDownloadProxyStoreError> {
    let mut all_proxy: Option<String> = None;
    let mut https_proxy: Option<String> = None;
    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = parse_key_value(line)?;
        match key.as_str() {
            "all_proxy" => all_proxy = Some(value),
            "https_proxy" => https_proxy = Some(value),
            other => {
                return Err(LocalDownloadProxyStoreError::new(format!(
                    "unknown local-download-proxy.toml field `{other}`"
                )));
            }
        }
    }
    Ok(LocalDownloadProxyConfig {
        all_proxy: all_proxy.and_then(|value| non_empty(Some(&value)).map(str::to_string)),
        https_proxy: https_proxy.and_then(|value| non_empty(Some(&value)).map(str::to_string)),
    })
}

fn parse_key_value(line: &str) -> Result<(String, String), LocalDownloadProxyStoreError> {
    let Some((key, value)) = line.split_once('=') else {
        return Err(LocalDownloadProxyStoreError::new(format!(
            "invalid local-download-proxy.toml line `{line}`"
        )));
    };
    let key = key.trim().to_string();
    let value = value.trim();
    if value.starts_with('"') {
        return Ok((key, parse_quoted(value)?));
    }
    Ok((key, value.to_string()))
}

fn parse_quoted(value: &str) -> Result<String, LocalDownloadProxyStoreError> {
    if !value.ends_with('"') || value.len() < 2 {
        return Err(LocalDownloadProxyStoreError::new(
            "unterminated local-download-proxy.toml string",
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
            "waitagent-local-download-proxy-{name}-{}-{}.toml",
            std::process::id(),
            std::thread::current()
                .name()
                .unwrap_or("test")
                .replace(":", "_")
        ))
    }

    #[test]
    fn default_path_uses_waitagent_home() {
        let path = LocalDownloadProxyConfig::default_path();
        assert!(path.ends_with(PathBuf::from(".waitagent/local-download-proxy.toml")));
    }

    #[test]
    fn round_trips_both_proxy_keys() {
        let path = unique_path("round-trip");
        let config = LocalDownloadProxyConfig {
            all_proxy: Some("socks5://127.0.0.1:7897".to_string()),
            https_proxy: Some("http://127.0.0.1:7890".to_string()),
        };
        config.save(&path).expect("save should succeed");
        let text = fs::read_to_string(&path).expect("file readable");
        assert!(text.contains("all_proxy = \"socks5://127.0.0.1:7897\""));
        assert!(text.contains("https_proxy = \"http://127.0.0.1:7890\""));

        let loaded = LocalDownloadProxyConfig::load(&path).expect("load should succeed");
        assert_eq!(loaded, Some(config));
        crate::infra::best_effort::remove_file(&path);
    }

    #[test]
    fn load_missing_file_yields_none() {
        let path = unique_path("missing");
        assert_eq!(LocalDownloadProxyConfig::load(&path).expect("ok"), None);
    }

    #[test]
    fn empty_config_round_trips_without_keys() {
        let path = unique_path("empty");
        let config = LocalDownloadProxyConfig::default();
        config.save(&path).expect("save should succeed");
        let text = fs::read_to_string(&path).expect("file readable");
        assert!(!text.contains("all_proxy ="));
        assert!(!text.contains("https_proxy ="));

        let loaded = LocalDownloadProxyConfig::load(&path).expect("load should succeed");
        assert_eq!(loaded, Some(config));
        assert!(!loaded.expect("present").has_proxy());
        crate::infra::best_effort::remove_file(&path);
    }

    #[test]
    fn empty_string_values_mean_no_proxy() {
        let path = unique_path("empty-values");
        fs::write(&path, "all_proxy = \"\"\nhttps_proxy = \"\"\n").expect("write should succeed");
        let loaded = LocalDownloadProxyConfig::load(&path).expect("load should succeed");
        let config = loaded.expect("present");
        assert_eq!(config.all_proxy, None);
        assert_eq!(config.https_proxy, None);
        assert!(!config.has_proxy());
        assert!(config.env_assignments().is_empty());
        crate::infra::best_effort::remove_file(&path);
    }

    #[test]
    fn rejects_unknown_keys() {
        let path = unique_path("unknown-key");
        fs::write(&path, "all_proxy = \"x\"\nsurprise = 1\n").expect("write should succeed");
        let error = LocalDownloadProxyConfig::load(&path).expect_err("unknown key should fail");
        assert!(error
            .to_string()
            .contains("unknown local-download-proxy.toml field"));
        crate::infra::best_effort::remove_file(&path);
    }

    #[test]
    fn env_assignments_cover_set_and_unset() {
        let none = LocalDownloadProxyConfig::default();
        assert!(none.env_assignments().is_empty());

        let all_only = LocalDownloadProxyConfig {
            all_proxy: Some("socks5://10.0.0.1:7897".to_string()),
            https_proxy: None,
        };
        assert_eq!(
            all_only.env_assignments(),
            vec![("all_proxy", "socks5://10.0.0.1:7897".to_string())]
        );

        let https_only = LocalDownloadProxyConfig {
            all_proxy: None,
            https_proxy: Some("http://10.0.0.1:7890".to_string()),
        };
        assert_eq!(
            https_only.env_assignments(),
            vec![("https_proxy", "http://10.0.0.1:7890".to_string())]
        );

        let both = LocalDownloadProxyConfig {
            all_proxy: Some("socks5://10.0.0.1:7897".to_string()),
            https_proxy: Some("http://10.0.0.1:7890".to_string()),
        };
        assert_eq!(
            both.env_assignments(),
            vec![
                ("all_proxy", "socks5://10.0.0.1:7897".to_string()),
                ("https_proxy", "http://10.0.0.1:7890".to_string()),
            ]
        );
        assert!(both.has_proxy());
    }

    #[test]
    fn escapes_special_characters() {
        let path = unique_path("escapes");
        // Quotes/backslashes exercise the escape table; the tab sits inside
        // the value so the load-time whitespace trim cannot touch it.
        let config = LocalDownloadProxyConfig {
            all_proxy: Some("http://user\"na\tme\\:p@proxy:7890".to_string()),
            https_proxy: None,
        };
        config.save(&path).expect("save should succeed");
        let loaded = LocalDownloadProxyConfig::load(&path).expect("load should succeed");
        assert_eq!(loaded, Some(config));
        crate::infra::best_effort::remove_file(&path);
    }
}
