//! Device fingerprint (issue #131 v3): SHA-256 over the stable signals a
//! public HTTP deployment can observe — client IP (through the
//! trusted-proxies rule), User-Agent, and four one-time JS probe values
//! (platform, timezone, language, screen) collected on the login page and
//! re-sent by the dashboard's heartbeat. MAC is unreachable at HTTP scope
//! and deliberately not used.
//!
//! The "same machine" constraint is literal (v3, user-confirmed): any
//! change in IP / UA / browser / network invalidates the session and forces
//! a fresh magic link. Missing probe values hash as empty strings, so a
//! no-JS login still yields a stable (weaker) fingerprint for its session.

use std::net::IpAddr;

use sha2::{Digest, Sha256};

/// The four probe values collected by the inline login/heartbeat script.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Probe {
    pub platform: String,
    pub timezone: String,
    pub language: String,
    pub screen: String,
}

impl Probe {
    /// Extracts the probe from submitted form/JSON fields; absent fields
    /// default to empty (documented missing-field policy).
    pub fn from_fields(
        platform: Option<&str>,
        timezone: Option<&str>,
        language: Option<&str>,
        screen: Option<&str>,
    ) -> Self {
        Self {
            platform: platform.unwrap_or_default().to_string(),
            timezone: timezone.unwrap_or_default().to_string(),
            language: language.unwrap_or_default().to_string(),
            screen: screen.unwrap_or_default().to_string(),
        }
    }
}

/// Computes the client IP for fingerprinting: the connection peer, unless
/// the peer is a configured trusted proxy — then the first
/// `X-Forwarded-For` entry. An empty proxy list (the default) means direct
/// connections only.
pub fn client_ip(peer: IpAddr, forwarded: Option<&str>, trusted: &[IpAddr]) -> IpAddr {
    if !trusted.contains(&peer) {
        return peer;
    }
    forwarded
        .and_then(|value| value.split(',').next())
        .map(str::trim)
        .and_then(|ip| ip.parse::<IpAddr>().ok())
        .unwrap_or(peer)
}

/// The fingerprint: SHA-256 over the six signals, newline-joined. Pure and
/// deterministic so tests pin exact digests.
pub fn compute(ip: &str, user_agent: &str, probe: &Probe) -> String {
    let mut hasher = Sha256::new();
    for field in [
        ip,
        user_agent,
        &probe.platform,
        &probe.timezone,
        &probe.language,
        &probe.screen,
    ] {
        hasher.update(field.as_bytes());
        hasher.update([0x0a]);
    }
    hex(&hasher.finalize())
}

/// Constant-time comparison for two fixed-length hex digests (a mismatch
/// must not leak how early it occurred).
pub fn constant_time_eq(a: &str, b: &str) -> bool {
    let a = a.as_bytes();
    let b = b.as_bytes();
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (left, right) in a.iter().zip(b.iter()) {
        diff |= left ^ right;
    }
    diff == 0
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    use std::fmt::Write;
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe() -> Probe {
        Probe {
            platform: "Linux x86_64".to_string(),
            timezone: "Asia/Shanghai".to_string(),
            language: "en-US".to_string(),
            screen: "1920x1080".to_string(),
        }
    }

    #[test]
    fn fingerprint_is_deterministic_and_field_sensitive() {
        let base = compute("203.0.113.7", "Mozilla/5.0", &probe());
        assert_eq!(base.len(), 64);
        assert_eq!(base, compute("203.0.113.7", "Mozilla/5.0", &probe()));

        let variants = [
            compute("203.0.113.8", "Mozilla/5.0", &probe()),
            compute("203.0.113.7", "Mozilla/6.0", &probe()),
            compute(
                "203.0.113.7",
                "Mozilla/5.0",
                &Probe::from_fields(
                    None,
                    Some("Asia/Shanghai"),
                    Some("en-US"),
                    Some("1920x1080"),
                ),
            ),
            compute(
                "203.0.113.7",
                "Mozilla/5.0",
                &Probe::from_fields(
                    Some("Linux x86_64"),
                    Some("UTC"),
                    Some("en-US"),
                    Some("1920x1080"),
                ),
            ),
        ];
        for variant in variants {
            assert_ne!(base, variant, "any single signal change must change the fp");
        }
    }

    #[test]
    fn missing_probe_fields_hash_stably() {
        let empty = compute("10.0.0.1", "UA", &Probe::default());
        assert_eq!(empty, compute("10.0.0.1", "UA", &Probe::default()));
        assert_ne!(
            empty,
            compute("10.0.0.1", "UA", &probe()),
            "probes contribute when present"
        );
    }

    #[test]
    fn constant_time_eq_only_true_for_exact_match() {
        assert!(constant_time_eq(&"a".repeat(64), &"a".repeat(64)));
        assert!(!constant_time_eq(&"a".repeat(64), &"b".repeat(64)));
        assert!(!constant_time_eq("ab", "ba"));
        assert!(!constant_time_eq("abc", "abcd"));
    }

    #[test]
    fn client_ip_honors_trusted_proxies_only() {
        let peer: IpAddr = "127.0.0.1".parse().unwrap();
        let real: IpAddr = "198.51.100.9".parse().unwrap();

        // Default (empty) trust list: the header is ignored.
        assert_eq!(
            client_ip(peer, Some("198.51.100.9"), &[]),
            peer,
            "untrusted proxy: peer addr wins"
        );
        // Trusted proxy: the forwarded value resolves (first entry).
        assert_eq!(
            client_ip(peer, Some("198.51.100.9, 10.0.0.1"), &[peer]),
            real
        );
        // Trusted proxy but garbage header: fall back to the peer.
        assert_eq!(client_ip(peer, Some("not-an-ip"), &[peer]), peer);
        assert_eq!(client_ip(peer, None, &[peer]), peer);
    }
}
