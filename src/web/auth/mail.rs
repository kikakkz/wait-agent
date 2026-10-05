//! The mail provider registry (issue #131 v4): the operator configures only
//! `admin_email` + `mail_auth_code`; the SMTP endpoint resolves from the
//! mailbox domain through this built-in table. Domains outside the table
//! require the `mail_custom_*` config keys (the design's `[mail.custom]`
//! escape hatch for self-hosted relays and corporate mail).

use crate::web::config::{MailCustom, MailTls, WebuiConfig};

/// A resolved SMTP endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SmtpEndpoint {
    pub host: String,
    pub port: u16,
    pub tls: MailTls,
    /// The AUTH LOGIN identity; the v4 design sends from the admin's own
    /// mailbox, so this is the admin email unless a custom override says
    /// otherwise.
    pub user: String,
}

/// Resolves the SMTP endpoint for `config`: the custom override wins when
/// present; otherwise the admin email's domain must be in the provider
/// table. `auth_code` rides in the message send, not here.
pub fn smtp_endpoint(config: &WebuiConfig) -> Result<SmtpEndpoint, String> {
    if let Some(MailCustom {
        host,
        port,
        tls,
        user,
    }) = &config.mail_custom
    {
        return Ok(SmtpEndpoint {
            host: host.clone(),
            port: *port,
            tls: *tls,
            user: user.clone(),
        });
    }
    let domain = config
        .admin_email
        .rsplit_once('@')
        .map(|(_local, domain)| domain.to_ascii_lowercase())
        .ok_or_else(|| format!("admin_email {:?} has no @ domain", config.admin_email))?;
    let (host, port, tls) = match domain.as_str() {
        "qq.com" => ("smtp.qq.com", 465, MailTls::Ssl),
        "163.com" => ("smtp.163.com", 465, MailTls::Ssl),
        "hotmail.com" | "outlook.com" => ("smtp.office365.com", 587, MailTls::StartTls),
        "gmail.com" => ("smtp.gmail.com", 465, MailTls::Ssl),
        other => {
            return Err(format!(
                "no built-in SMTP provider for domain {other:?}; set mail_custom_host/port/tls/user in webui.toml"
            ))
        }
    };
    Ok(SmtpEndpoint {
        host: host.to_string(),
        port,
        tls,
        user: config.admin_email.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_for(email: &str) -> WebuiConfig {
        WebuiConfig {
            admin_email: email.to_string(),
            mail_auth_code: "code".to_string(),
            public_base_url: "https://dash.example".to_string(),
            trusted_proxies: Vec::new(),
            mail_custom: None,
        }
    }

    #[test]
    fn provider_table_covers_every_signed_up_domain() {
        let cases = [
            ("admin@qq.com", "smtp.qq.com", 465, MailTls::Ssl),
            ("admin@163.com", "smtp.163.com", 465, MailTls::Ssl),
            (
                "admin@hotmail.com",
                "smtp.office365.com",
                587,
                MailTls::StartTls,
            ),
            (
                "admin@outlook.com",
                "smtp.office365.com",
                587,
                MailTls::StartTls,
            ),
            ("admin@gmail.com", "smtp.gmail.com", 465, MailTls::Ssl),
        ];
        for (email, host, port, tls) in cases {
            let endpoint = smtp_endpoint(&config_for(email)).expect(email);
            assert_eq!(endpoint.host, host, "{email}");
            assert_eq!(endpoint.port, port, "{email}");
            assert_eq!(endpoint.tls, tls, "{email}");
            assert_eq!(endpoint.user, email, "AUTH identity is the admin email");
        }
    }

    #[test]
    fn domain_match_is_case_insensitive() {
        let endpoint = smtp_endpoint(&config_for("Admin@QQ.com")).expect("case folds");
        assert_eq!(endpoint.host, "smtp.qq.com");
    }

    #[test]
    fn unknown_domain_needs_the_custom_escape_hatch() {
        let error =
            smtp_endpoint(&config_for("admin@corp.example")).expect_err("unknown domain must fail");
        assert!(error.contains("mail_custom"), "{error}");
    }

    #[test]
    fn custom_override_wins_over_the_table() {
        let mut config = config_for("admin@qq.com");
        config.mail_custom = Some(MailCustom {
            host: "127.0.0.1".to_string(),
            port: 2525,
            tls: MailTls::None,
            user: "stub@local".to_string(),
        });
        let endpoint = smtp_endpoint(&config).expect("custom resolves");
        assert_eq!(
            endpoint,
            SmtpEndpoint {
                host: "127.0.0.1".to_string(),
                port: 2525,
                tls: MailTls::None,
                user: "stub@local".to_string(),
            }
        );
    }
}
