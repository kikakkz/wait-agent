//! The WebUI deployment config: `~/.waitagent/webui.toml` (issue #131
//! slice 3, security design v2/v4). The operator fills in the admin email,
//! the mailbox app password ("授权码"), and the public base URL at deploy
//! time; the SMTP host/port/TLS resolve from a built-in provider table by
//! the email domain, with `mail_custom_*` keys as the escape hatch for
//! domains outside the table (the design's `[mail.custom]`, expressed in
//! this codebase's flat key = value store idiom).
//!
//! Like the other hand-rolled stores (`relay_toml_store`), unknown keys are
//! rejected so a stray edit surfaces instead of being silently ignored.

use std::fmt;
use std::fs;
use std::io;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use crate::host::ssh::remote_host_home::waitagent_home;

/// The WebUI deployment config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebuiConfig {
    /// The single administrator's email; magic links are only ever sent
    /// here (typing any other address gets the same neutral answer, so the
    /// endpoint cannot be used to probe or bombard third parties).
    pub admin_email: String,
    /// The mailbox app password / authorization code (v4: the operator only
    /// ever fills email + code; provider parameters come from the table).
    pub mail_auth_code: String,
    /// External base URL of the dashboard; magic links are built as
    /// `{public_base_url}/auth/magic?token=...`.
    pub public_base_url: String,
    /// Reverse proxies trusted for `X-Forwarded-For` resolution (default
    /// empty = direct connections only).
    pub trusted_proxies: Vec<IpAddr>,
    /// Custom SMTP override for domains outside the provider table.
    pub mail_custom: Option<MailCustom>,
}

/// Full SMTP parameters for a mailbox domain the provider table does not
/// know (self-hosted relays, corporate mail).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailCustom {
    pub host: String,
    pub port: u16,
    pub tls: MailTls,
    pub user: String,
}

/// Transport security for the SMTP conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MailTls {
    /// TLS from the first byte (SMTPS, e.g. port 465).
    Ssl,
    /// Plaintext greeting, then STARTTLS upgrade (e.g. port 587).
    StartTls,
    /// No TLS; loopback stubs and test doubles only.
    None,
}

impl WebuiConfig {
    /// Returns the default path: `waitagent_home()/webui.toml`.
    pub fn default_path() -> PathBuf {
        waitagent_home().join("webui.toml")
    }

    /// Loads the config from `path`; a missing file is an error with
    /// guidance (auth cannot work without the deployment-time values).
    pub fn load(path: &Path) -> Result<Self, WebuiConfigError> {
        let text = fs::read_to_string(path).map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                WebuiConfigError::Missing(path.to_path_buf())
            } else {
                WebuiConfigError::Io(error)
            }
        })?;
        parse_webui_toml(&text)
    }

    /// Writes the config to `path` (tmp + rename), creating the parent.
    /// Test-only: production never persists the operator-authored config.
    #[cfg(test)]
    pub fn save(&self, path: &Path) -> Result<(), WebuiConfigError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(WebuiConfigError::Io)?;
        }
        let tmp_path = path.with_extension("tmp");
        fs::write(&tmp_path, serialize_webui_toml(self)).map_err(WebuiConfigError::Io)?;
        fs::rename(&tmp_path, path).map_err(WebuiConfigError::Io)
    }
}

/// Errors of the WebUI config store.
#[derive(Debug)]
pub enum WebuiConfigError {
    Io(io::Error),
    Missing(PathBuf),
    Invalid(String),
}

impl fmt::Display for WebuiConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "io error: {error}"),
            Self::Missing(path) => write!(
                f,
                "webui config {} is missing; create it with admin_email, mail_auth_code, and public_base_url (see `waitagent web serve` docs / issue #131 slice 3)",
                path.display()
            ),
            Self::Invalid(message) => write!(f, "invalid webui config: {message}"),
        }
    }
}

impl std::error::Error for WebuiConfigError {}

#[cfg(test)]
fn serialize_webui_toml(config: &WebuiConfig) -> String {
    let mut out = String::with_capacity(256);
    out.push_str("# WaitAgent WebUI deployment config (issue #131)\n");
    push_string(&mut out, "admin_email", &config.admin_email);
    push_string(&mut out, "mail_auth_code", &config.mail_auth_code);
    push_string(&mut out, "public_base_url", &config.public_base_url);
    let proxies = config
        .trusted_proxies
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(",");
    push_string(&mut out, "trusted_proxies", &proxies);
    if let Some(custom) = &config.mail_custom {
        push_string(&mut out, "mail_custom_host", &custom.host);
        out.push_str("mail_custom_port = ");
        out.push_str(&custom.port.to_string());
        out.push('\n');
        push_string(
            &mut out,
            "mail_custom_tls",
            match custom.tls {
                MailTls::Ssl => "ssl",
                MailTls::StartTls => "starttls",
                MailTls::None => "none",
            },
        );
        push_string(&mut out, "mail_custom_user", &custom.user);
    }
    out
}

#[cfg(test)]
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

fn parse_webui_toml(text: &str) -> Result<WebuiConfig, WebuiConfigError> {
    let mut admin_email: Option<String> = None;
    let mut mail_auth_code: Option<String> = None;
    let mut public_base_url: Option<String> = None;
    let mut trusted_proxies: Option<Vec<IpAddr>> = None;
    let mut mail_custom_host: Option<String> = None;
    let mut mail_custom_port: Option<u16> = None;
    let mut mail_custom_tls: Option<MailTls> = None;
    let mut mail_custom_user: Option<String> = None;

    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = parse_key_value(line)?;
        match key.as_str() {
            "admin_email" => admin_email = Some(value),
            "mail_auth_code" => mail_auth_code = Some(value),
            "public_base_url" => public_base_url = Some(value),
            "trusted_proxies" => {
                trusted_proxies = Some(if value.trim().is_empty() {
                    Vec::new()
                } else {
                    value
                        .split(',')
                        .map(str::trim)
                        .map(|ip| {
                            ip.parse::<IpAddr>().map_err(|_| {
                                WebuiConfigError::Invalid(format!(
                                    "trusted_proxies entry {ip:?} is not an IP address"
                                ))
                            })
                        })
                        .collect::<Result<Vec<_>, _>>()?
                });
            }
            "mail_custom_host" => mail_custom_host = Some(value),
            "mail_custom_port" => {
                mail_custom_port = Some(value.parse::<u16>().map_err(|_| {
                    WebuiConfigError::Invalid("mail_custom_port must be a port number".to_string())
                })?)
            }
            "mail_custom_tls" => {
                mail_custom_tls = Some(match value.as_str() {
                    "ssl" => MailTls::Ssl,
                    "starttls" => MailTls::StartTls,
                    "none" => MailTls::None,
                    other => {
                        return Err(WebuiConfigError::Invalid(format!(
                            "mail_custom_tls must be ssl|starttls|none, got {other:?}"
                        )))
                    }
                })
            }
            "mail_custom_user" => mail_custom_user = Some(value),
            other => {
                return Err(WebuiConfigError::Invalid(format!(
                    "unknown webui.toml field `{other}`"
                )))
            }
        }
    }

    let admin_email = admin_email
        .filter(|value| !value.is_empty())
        .ok_or_else(|| WebuiConfigError::Invalid("webui.toml lacks `admin_email`".to_string()))?;
    let mail_auth_code = mail_auth_code
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            WebuiConfigError::Invalid("webui.toml lacks `mail_auth_code`".to_string())
        })?;
    let public_base_url = public_base_url
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            WebuiConfigError::Invalid("webui.toml lacks `public_base_url`".to_string())
        })?;
    if !public_base_url.starts_with("http://") && !public_base_url.starts_with("https://") {
        return Err(WebuiConfigError::Invalid(
            "public_base_url must start with http:// or https://".to_string(),
        ));
    }

    let mail_custom = match (
        mail_custom_host,
        mail_custom_port,
        mail_custom_tls,
        mail_custom_user,
    ) {
        (None, None, None, None) => None,
        (Some(host), Some(port), Some(tls), Some(user)) => Some(MailCustom {
            host,
            port,
            tls,
            user,
        }),
        _ => {
            return Err(WebuiConfigError::Invalid(
                "mail_custom_* keys must be all present or all absent".to_string(),
            ))
        }
    };

    Ok(WebuiConfig {
        admin_email,
        mail_auth_code,
        public_base_url: public_base_url.trim_end_matches('/').to_string(),
        trusted_proxies: trusted_proxies.unwrap_or_default(),
        mail_custom,
    })
}

fn parse_key_value(line: &str) -> Result<(String, String), WebuiConfigError> {
    let Some((key, value)) = line.split_once('=') else {
        return Err(WebuiConfigError::Invalid(format!(
            "invalid webui.toml line `{line}`"
        )));
    };
    let key = key.trim().to_string();
    let value = value.trim();
    if value.starts_with('"') {
        if !value.ends_with('"') || value.len() < 2 {
            return Err(WebuiConfigError::Invalid(
                "unterminated webui.toml string".to_string(),
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
        return Ok((key, out));
    }
    Ok((key, value.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_path(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "waitagent-webui-config-{name}-{}-{}.toml",
            std::process::id(),
            std::thread::current()
                .name()
                .unwrap_or("test")
                .replace(":", "_")
        ));
        let _ = fs::remove_file(&path);
        path
    }

    fn sample() -> WebuiConfig {
        WebuiConfig {
            admin_email: "admin@example.com".to_string(),
            mail_auth_code: "secret-code".to_string(),
            public_base_url: "https://dash.example.com".to_string(),
            trusted_proxies: vec!["127.0.0.1".parse().expect("ip")],
            mail_custom: Some(MailCustom {
                host: "smtp.corp.example".to_string(),
                port: 2525,
                tls: MailTls::StartTls,
                user: "admin@corp.example".to_string(),
            }),
        }
    }

    #[test]
    fn round_trips_the_full_config() {
        let path = unique_path("round-trip");
        let config = sample();
        config.save(&path).expect("save should succeed");
        let loaded = WebuiConfig::load(&path).expect("load should succeed");
        assert_eq!(loaded, config);
        crate::infra::best_effort::remove_file(&path);
    }

    #[test]
    fn round_trips_without_optional_sections() {
        let path = unique_path("minimal");
        let config = WebuiConfig {
            trusted_proxies: Vec::new(),
            mail_custom: None,
            ..sample()
        };
        config.save(&path).expect("save should succeed");
        let loaded = WebuiConfig::load(&path).expect("load should succeed");
        assert_eq!(loaded, config);
        assert!(loaded.trusted_proxies.is_empty());
        assert!(loaded.mail_custom.is_none());
        crate::infra::best_effort::remove_file(&path);
    }

    #[test]
    fn missing_file_names_the_fix() {
        let path = unique_path("missing");
        let error = WebuiConfig::load(&path).expect_err("a missing config must fail");
        assert!(
            error.to_string().contains("admin_email"),
            "guidance names the keys: {error}"
        );
    }

    #[test]
    fn rejects_missing_required_keys_and_unknown_keys() {
        let dir = std::env::temp_dir().join(format!(
            "waitagent-webui-config-bad-{}-{}",
            std::process::id(),
            std::thread::current()
                .name()
                .unwrap_or("test")
                .replace(":", "_")
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("dir");

        let missing = dir.join("missing-keys.toml");
        fs::write(
            &missing,
            "admin_email = \"a@b.c\"\npublic_base_url = \"https://x.example\"\n",
        )
        .expect("write");
        let error = WebuiConfig::load(&missing).expect_err("incomplete config must fail");
        assert!(error.to_string().contains("mail_auth_code"), "{error}");

        let unknown = dir.join("unknown-key.toml");
        fs::write(
            &unknown,
            "admin_email = \"a@b.c\"\nmail_auth_code = \"x\"\npublic_base_url = \"https://x.example\"\nsurprise = 1\n",
        )
        .expect("write");
        let error = WebuiConfig::load(&unknown).expect_err("unknown keys must fail");
        assert!(error.to_string().contains("surprise"), "{error}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_partial_custom_and_bad_base_url() {
        let dir = std::env::temp_dir().join(format!(
            "waitagent-webui-config-partial-{}-{}",
            std::process::id(),
            std::thread::current()
                .name()
                .unwrap_or("test")
                .replace(":", "_")
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("dir");

        let partial = dir.join("partial.toml");
        fs::write(
            &partial,
            "admin_email = \"a@b.c\"\nmail_auth_code = \"x\"\npublic_base_url = \"https://x.example\"\nmail_custom_host = \"smtp.corp\"\n",
        )
        .expect("write");
        let error = WebuiConfig::load(&partial).expect_err("partial custom must fail");
        assert!(error.to_string().contains("mail_custom"), "{error}");

        let bad_url = dir.join("bad-url.toml");
        fs::write(
            &bad_url,
            "admin_email = \"a@b.c\"\nmail_auth_code = \"x\"\npublic_base_url = \"not-a-url\"\n",
        )
        .expect("write");
        let error = WebuiConfig::load(&bad_url).expect_err("bad base url must fail");
        assert!(error.to_string().contains("http"), "{error}");

        let _ = fs::remove_dir_all(&dir);
    }
}
