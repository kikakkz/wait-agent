use std::fmt;
use std::fs;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
/// `coordination.toml`: listen, embedded relay, presence, admin mailbox, operators, token TTL.
pub struct CoordinationConfig {
    pub listen: Option<SocketAddr>,
    pub relay: Option<bool>,
    pub relay_url: Option<String>,
    pub presence_timeout_secs: Option<u64>,
    pub admin_email: Option<String>,
    pub mail_auth_code: Option<String>,
    pub operators: Vec<String>,
    pub token_ttl_secs: Option<u64>,
}

impl CoordinationConfig {
    /// Effective token TTL (default 15 minutes).
    pub fn token_ttl(&self) -> std::time::Duration {
        std::time::Duration::from_secs(
            self.token_ttl_secs
                .unwrap_or(crate::infra::coordination::DEFAULT_TOKEN_TTL_SECS),
        )
    }

    /// Effective presence timeout (default three missed 15 s heartbeats).
    pub fn presence_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(
            self.presence_timeout_secs
                .unwrap_or(crate::infra::coordination::presence::DEFAULT_PRESENCE_TIMEOUT_SECS),
        )
    }

    /// Whether the embedded iroh relay starts with `coordination serve`.
    pub fn relay_enabled(&self) -> bool {
        self.relay.unwrap_or(true)
    }

    /// Loads the config; a missing file yields the defaults.
    pub fn load(path: &Path) -> Result<Option<Self>, CoordinationTomlError> {
        if !path.is_file() {
            return Ok(None);
        }
        let text = fs::read_to_string(path).map_err(CoordinationTomlError::io)?;
        parse_config(&text).map(Some)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Errors of the coordination config store.
pub struct CoordinationTomlError {
    message: String,
}

impl CoordinationTomlError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    fn io(error: io::Error) -> Self {
        Self::new(error.to_string())
    }
}

impl fmt::Display for CoordinationTomlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for CoordinationTomlError {}

fn parse_config(text: &str) -> Result<CoordinationConfig, CoordinationTomlError> {
    let mut config = CoordinationConfig::default();
    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = parse_key_value(line)?;
        match key.as_str() {
            "listen" => {
                config.listen = Some(value.parse::<SocketAddr>().map_err(|_| {
                    CoordinationTomlError::new(format!(
                        "coordination.toml `listen` must be a socket address, got {value:?}"
                    ))
                })?);
            }
            "relay" => {
                config.relay = Some(parse_bool(&value, "relay")?);
            }
            "relay_url" => config.relay_url = non_empty(value),
            "presence_timeout_secs" => {
                let secs: u64 = value.parse().map_err(|_| {
                    CoordinationTomlError::new(format!(
                        "coordination.toml `presence_timeout_secs` must be a whole number, got {value:?}"
                    ))
                })?;
                if secs == 0 {
                    return Err(CoordinationTomlError::new(
                        "coordination.toml `presence_timeout_secs` must be greater than 0",
                    ));
                }
                config.presence_timeout_secs = Some(secs);
            }
            "admin_email" => config.admin_email = non_empty(value),
            "mail_auth_code" => config.mail_auth_code = non_empty(value),
            "operators" => config.operators = parse_list(&value)?,
            "token_ttl_secs" => {
                let secs: u64 = value.parse().map_err(|_| {
                    CoordinationTomlError::new(format!(
                        "coordination.toml `token_ttl_secs` must be a whole number, got {value:?}"
                    ))
                })?;
                if secs == 0 {
                    return Err(CoordinationTomlError::new(
                        "coordination.toml `token_ttl_secs` must be greater than 0",
                    ));
                }
                config.token_ttl_secs = Some(secs);
            }
            other => {
                return Err(CoordinationTomlError::new(format!(
                    "unknown coordination.toml field `{other}`"
                )));
            }
        }
    }
    Ok(config)
}

fn non_empty(value: String) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn parse_bool(value: &str, key: &str) -> Result<bool, CoordinationTomlError> {
    match value.trim() {
        "true" => Ok(true),
        "false" => Ok(false),
        other => Err(CoordinationTomlError::new(format!(
            "coordination.toml `{key}` must be true or false, got {other:?}"
        ))),
    }
}

fn parse_list(value: &str) -> Result<Vec<String>, CoordinationTomlError> {
    let trimmed = value.trim();
    let inner = trimmed
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .ok_or_else(|| {
            CoordinationTomlError::new(format!(
                "coordination.toml list must be [a, b], got {trimmed:?}"
            ))
        })?;
    Ok(inner
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(|entry| entry.trim_matches('"').to_string())
        .collect())
}

fn parse_key_value(line: &str) -> Result<(String, String), CoordinationTomlError> {
    let Some((key, value)) = line.split_once('=') else {
        return Err(CoordinationTomlError::new(format!(
            "invalid coordination.toml line `{line}`"
        )));
    };
    let key = key.trim().to_string();
    let value = value.trim();
    if value.starts_with('"') {
        return Ok((key, parse_quoted(value)?));
    }
    Ok((key, value.to_string()))
}

fn parse_quoted(value: &str) -> Result<String, CoordinationTomlError> {
    if !value.ends_with('"') || value.len() < 2 {
        return Err(CoordinationTomlError::new(
            "unterminated coordination.toml string",
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

/// Default path: `~/.waitagent/coordination/coordination.toml`.
pub fn default_config_path() -> PathBuf {
    crate::infra::coordination::default_home().join("coordination.toml")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// One SMTP provider preset keyed by mailbox domain.
pub struct SmtpPreset {
    pub domain: &'static str,
    pub host: &'static str,
    pub port: u16,
    pub tls: crate::web::config::MailTls,
}

/// Data-driven SMTP provider table; add a row to support a new domain.
pub const SMTP_PRESETS: &[SmtpPreset] = &[
    SmtpPreset {
        domain: "qq.com",
        host: "smtp.qq.com",
        port: 465,
        tls: crate::web::config::MailTls::Ssl,
    },
    SmtpPreset {
        domain: "163.com",
        host: "smtp.163.com",
        port: 465,
        tls: crate::web::config::MailTls::Ssl,
    },
    SmtpPreset {
        domain: "hotmail.com",
        host: "smtp.office365.com",
        port: 587,
        tls: crate::web::config::MailTls::StartTls,
    },
    SmtpPreset {
        domain: "outlook.com",
        host: "smtp.office365.com",
        port: 587,
        tls: crate::web::config::MailTls::StartTls,
    },
    SmtpPreset {
        domain: "gmail.com",
        host: "smtp.gmail.com",
        port: 465,
        tls: crate::web::config::MailTls::Ssl,
    },
];

/// Resolves the preset for the mailbox domain (case-insensitive).
pub fn smtp_endpoint(admin_email: &str) -> Result<SmtpPreset, String> {
    let domain = admin_email
        .rsplit_once('@')
        .map(|(_, domain)| domain.trim().to_ascii_lowercase())
        .ok_or_else(|| format!("admin email {admin_email:?} has no @ domain"))?;
    SMTP_PRESETS
        .iter()
        .copied()
        .find(|preset| preset.domain == domain)
        .ok_or_else(|| {
            format!("no SMTP preset for domain {domain:?}; supported domains: qq.com, 163.com, hotmail.com, outlook.com, gmail.com")
        })
}

/// Mails the enrollment magic link to the admin mailbox; errors are explicit.
pub async fn send_enroll_magic_link(
    config: &CoordinationConfig,
    request: &crate::infra::coordination::enrollment::JoinRequest,
) -> Result<(), String> {
    let (email, code) = match (&config.admin_email, &config.mail_auth_code) {
        (Some(email), Some(code)) => (email, code),
        _ => return Err("coordination.toml has no admin_email/mail_auth_code".to_string()),
    };
    let preset = smtp_endpoint(email)?;
    let message = crate::web::auth::smtp::MailMessage {
        from: email.clone(),
        to: email.clone(),
        subject: format!(
            "[waitagent coordination] enrollment request — device {}",
            &request.node_id[..request.node_id.len().min(8)]
        ),
        body: format!(
            "Node {} requested enrollment (request {}, expires at {}).\n\nApprove with:\n  waitagent coordination approve {}\nThen forward the printed link to the applicant.\n",
            request.node_id,
            request.id,
            request.expires_at,
            request.id
        ),
    };
    crate::web::auth::smtp::send(preset.host, preset.port, preset.tls, email, code, &message)
        .await
        .map_err(|error| error.to_string())
}

/// Mails the dashboard magic login link to the admin mailbox.
pub fn dashboard_login_mail(
    admin_email: &str,
    login_url: &str,
    expires_at: u64,
) -> crate::web::auth::smtp::MailMessage {
    crate::web::auth::smtp::MailMessage {
        from: admin_email.to_string(),
        to: admin_email.to_string(),
        subject: "[waitagent coordination] dashboard login link".to_string(),
        body: format!(
            "Open this link within 15 minutes on the device that requested it:\n\n  {login_url}\n\nThe link expires at {expires_at} and works only from the requesting device.\n"
        ),
    }
}

/// The `waitagent://join?token=…` link for an approved request.
pub fn magic_link(request: &crate::infra::coordination::enrollment::JoinRequest) -> String {
    format!("waitagent://join?token={}", request.token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_round_trips_and_defaults() {
        let config = parse_config(
            "listen = \"0.0.0.0:7479\"\nadmin_email = \"admin@qq.com\"\nmail_auth_code = \"code\"\noperators = [\"op1\", \"op2\"]\ntoken_ttl_secs = 600\n",
        )
        .unwrap();
        assert_eq!(config.listen, Some("0.0.0.0:7479".parse().unwrap()));
        assert_eq!(config.admin_email.as_deref(), Some("admin@qq.com"));
        assert_eq!(config.operators, vec!["op1".to_string(), "op2".to_string()]);
        assert_eq!(config.token_ttl(), std::time::Duration::from_secs(600));

        let defaults = parse_config("").unwrap();
        assert_eq!(
            defaults.token_ttl(),
            std::time::Duration::from_secs(crate::infra::coordination::DEFAULT_TOKEN_TTL_SECS)
        );
        assert!(defaults.operators.is_empty());
    }

    #[test]
    fn config_rejects_unknown_keys_and_bad_values() {
        assert!(parse_config("surprise = 1\n").is_err());
        assert!(parse_config("listen = \"nope\"\n").is_err());
        assert!(parse_config("token_ttl_secs = 0\n").is_err());
        assert!(parse_config("operators = \"op1\"\n").is_err());
        assert!(parse_config("relay = maybe\n").is_err());
        assert!(parse_config("presence_timeout_secs = 0\n").is_err());
    }

    #[test]
    fn relay_and_presence_config_defaults_and_parsing() {
        let defaults = parse_config("").unwrap();
        assert!(defaults.relay_enabled());
        assert_eq!(
            defaults.presence_timeout(),
            std::time::Duration::from_secs(
                crate::infra::coordination::presence::DEFAULT_PRESENCE_TIMEOUT_SECS
            )
        );

        let config = parse_config(
            "relay = false\nrelay_url = \"https://relay.example\"\npresence_timeout_secs = 90\n",
        )
        .unwrap();
        assert!(!config.relay_enabled());
        assert_eq!(config.relay_url.as_deref(), Some("https://relay.example"));
        assert_eq!(
            config.presence_timeout(),
            std::time::Duration::from_secs(90)
        );
    }

    #[test]
    fn smtp_presets_resolve_by_domain_case_insensitively() {
        let preset = smtp_endpoint("Admin@QQ.com").unwrap();
        assert_eq!(preset.host, "smtp.qq.com");
        assert_eq!(preset.port, 465);
        let preset = smtp_endpoint("a@outlook.com").unwrap();
        assert_eq!(preset.tls, crate::web::config::MailTls::StartTls);
        assert!(smtp_endpoint("a@corp.example").is_err());
        assert!(smtp_endpoint("not-an-email").is_err());
    }
}
