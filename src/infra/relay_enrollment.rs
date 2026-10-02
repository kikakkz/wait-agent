//! Relay enrollment: invite-token mint/redeem (docs/relay-design.md 身份认证
//! 与入网) and the whitelist file operations behind `relay invite` / `relay
//! join` / `relay remove`.
//!
//! Tokens are opaque 256-bit random strings minted over the relay's local
//! admin socket. One-time (invite) tokens are consumed by any redeem attempt,
//! including rejected ones (fail-closed: a leaked token cannot be replayed
//! after a clock reset). Deploy tokens minted for batch provisioning survive
//! redemption until expiry. Token state persists to a JSON file under
//! `waitagent_home()` so a relay restart keeps minted-but-unused tokens; the
//! load path sweeps expired entries.
//!
//! `handle_enrollment_frame` holds the whole enrollment protocol decision so
//! `relay_server` stays a thin accept/read/respond shell around it.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::{STANDARD as BASE64_STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::infra::error_log::ERROR_LOG;
use crate::infra::relay_mux::frame::Frame;
use crate::infra::relay_routing::error_code;

/// Default TTL for one-time invite tokens.
// #35 surfaces these via relay.toml token parameters.
pub const DEFAULT_INVITE_TTL: Duration = Duration::from_secs(15 * 60);

/// Default TTL for long-lived deploy tokens (batch provisioning).
// #35 surfaces these via relay.toml token parameters.
pub const DEFAULT_DEPLOY_TTL: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// A freshly minted enrollment token as handed to the operator.
pub struct MintedToken {
    /// The raw token to pass to `relay join`; shown once, never persisted
    /// in plaintext outside the token store file.
    pub token: String,
    /// Unix seconds at which the token stops redeeming.
    pub expires_at_unix: u64,
    /// Whether the token is consumed by its first redeem attempt.
    pub one_time: bool,
}

/// Result of redeeming a token against the store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedeemOutcome {
    /// The token is valid; `one_time` mirrors the record's kind.
    EnrollOk { one_time: bool },
    /// The token is unknown or was already consumed.
    Invalid,
    /// The token is known but past its expiry.
    Expired,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TokenRecord {
    /// The raw token; kept inside the record because the map key is its
    /// hash — persistence and expiry reporting need the original value.
    token: String,
    expires_at_unix: u64,
    one_time: bool,
}

/// In-memory token store keyed by the SHA-256 hex of the token (the raw
/// token never becomes a map key, so token material is not duplicated in
/// memory more than necessary).
pub struct EnrollmentTokenStore {
    tokens: Mutex<HashMap<String, TokenRecord>>,
}

impl Default for EnrollmentTokenStore {
    fn default() -> Self {
        Self::new()
    }
}

impl EnrollmentTokenStore {
    pub fn new() -> Self {
        Self {
            tokens: Mutex::new(HashMap::new()),
        }
    }

    /// Mints a token with the given kind and TTL; returns the raw token plus
    /// its expiry. The token is immediately redeemable.
    pub fn mint(&self, one_time: bool, ttl: Duration) -> MintedToken {
        let mut bytes = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut bytes);
        let token = URL_SAFE_NO_PAD.encode(bytes);
        let expires_at_unix = now_unix() + ttl.as_secs();
        self.tokens
            .lock()
            .expect("enrollment token store lock poisoned")
            .insert(
                token_key(&token),
                TokenRecord {
                    token: token.clone(),
                    expires_at_unix,
                    one_time,
                },
            );
        MintedToken {
            token,
            expires_at_unix,
            one_time,
        }
    }

    /// Redeems `token` at `now_unix`. One-time tokens are consumed by any
    /// redeem attempt, including rejected ones (fail-closed); deploy tokens
    /// survive redemption until expiry, after which an expired redeem sweeps
    /// the dead record.
    pub fn redeem(&self, token: &str, now_unix: u64) -> RedeemOutcome {
        let key = token_key(token);
        let mut tokens = self
            .tokens
            .lock()
            .expect("enrollment token store lock poisoned");
        let Some(record) = tokens.get(&key).cloned() else {
            return RedeemOutcome::Invalid;
        };
        if record.one_time {
            tokens.remove(&key);
        }
        if record.expires_at_unix <= now_unix {
            if !record.one_time {
                tokens.remove(&key);
            }
            return RedeemOutcome::Expired;
        }
        RedeemOutcome::EnrollOk {
            one_time: record.one_time,
        }
    }

    /// Persists every unexpired token to `path` as JSON (tmp file + rename so
    /// a crash mid-write cannot leave a truncated store).
    pub fn persist(&self, path: &Path) -> io::Result<()> {
        let now = now_unix();
        let records: Vec<TokenRecord> = self
            .tokens
            .lock()
            .expect("enrollment token store lock poisoned")
            .values()
            .filter(|record| record.expires_at_unix > now)
            .cloned()
            .collect();
        let body = serde_json::to_vec(&records)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp_path = path.with_extension("tmp");
        fs::write(&tmp_path, body)?;
        fs::rename(&tmp_path, path)
    }

    /// Loads the store from `path`; a missing file loads as an empty store.
    /// Expired entries are swept on load. Malformed JSON is an error so a
    /// corrupt store surfaces instead of silently minting over it.
    pub fn load(path: &Path) -> io::Result<Self> {
        let store = Self::new();
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(store),
            Err(error) => return Err(error),
        };
        let records: Vec<TokenRecord> = serde_json::from_str(&text)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
        let now = now_unix();
        {
            let mut tokens = store
                .tokens
                .lock()
                .expect("enrollment token store lock poisoned");
            for record in records {
                if record.expires_at_unix <= now {
                    continue;
                }
                tokens.insert(token_key(&record.token), record);
            }
        }
        Ok(store)
    }
}

/// Decides the enrollment session's response for one first frame: redeem the
/// token, whitelist the peer, and answer with the relay fingerprint — or
/// answer with a structured error. Returns `None` when the connection should
/// just close after logging (an internal failure writing the whitelist).
pub fn handle_enrollment_frame(
    frame: Frame,
    peer_fingerprint: &str,
    peer_cert_pem: &str,
    store: &EnrollmentTokenStore,
    whitelist_dir: &Path,
    relay_fingerprint: &str,
    now_unix: u64,
) -> Option<Frame> {
    let token = match frame {
        Frame::Enroll { token } => token,
        other => {
            ERROR_LOG.log_error(format!(
                "[relay-enroll] first frame must be Enroll, got {other:?}"
            ));
            return Some(Frame::Error {
                stream_id: 0,
                code: error_code::TOKEN_INVALID,
                message: "first frame must be Enroll".to_string(),
            });
        }
    };
    match store.redeem(&token, now_unix) {
        RedeemOutcome::EnrollOk { .. } => {
            if let Err(error) = add_authorized_node(whitelist_dir, peer_fingerprint, peer_cert_pem)
            {
                ERROR_LOG.log_error(format!(
                    "[relay-enroll] whitelisting {peer_fingerprint} failed: {error}"
                ));
                return None;
            }
            Some(Frame::EnrollResponse {
                fingerprint: relay_fingerprint.to_string(),
            })
        }
        RedeemOutcome::Invalid => Some(Frame::Error {
            stream_id: 0,
            code: error_code::TOKEN_INVALID,
            message: "enrollment token is unknown or already used".to_string(),
        }),
        RedeemOutcome::Expired => Some(Frame::Error {
            stream_id: 0,
            code: error_code::TOKEN_EXPIRED,
            message: "enrollment token has expired".to_string(),
        }),
    }
}

/// Adds (or refreshes) a whitelist entry: `dir/<lowercase fingerprint>`
/// holding the node's certificate PEM. Creates `dir` when missing. The file
/// layout matches `relay_server::authorized_node_fingerprints`, which lists
/// the lowercase file names.
pub fn add_authorized_node(dir: &Path, fingerprint: &str, cert_pem: &str) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    fs::write(dir.join(fingerprint.to_lowercase()), cert_pem)
}

/// Removes the whitelist entry for `fingerprint`; returns whether a file was
/// actually removed.
pub fn remove_authorized_node(dir: &Path, fingerprint: &str) -> io::Result<bool> {
    match fs::remove_file(dir.join(fingerprint.to_lowercase())) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// PEM-encodes a DER certificate with the standard 64-column base64 layout.
pub(crate) fn pem_encode_cert(cert_der: &[u8]) -> String {
    let encoded = BASE64_STANDARD.encode(cert_der);
    let mut out = String::with_capacity(encoded.len() + 64);
    out.push_str("-----BEGIN CERTIFICATE-----\n");
    for chunk in encoded.as_bytes().chunks(64) {
        // Base64 output is pure ASCII, so the chunk is always valid UTF-8.
        out.push_str(std::str::from_utf8(chunk).expect("base64 is ascii"));
        out.push('\n');
    }
    out.push_str("-----END CERTIFICATE-----\n");
    out
}

/// Enrollment token store persistence path under `waitagent_home()`.
pub fn default_token_store_path() -> std::path::PathBuf {
    crate::host::ssh::remote_host_home::waitagent_home().join("relay-enroll-tokens.json")
}

fn token_key(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    let mut out = String::with_capacity(digest.len() * 2);
    use std::fmt::Write;
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "waitagent-relay-enroll-{name}-{}-{}",
            std::process::id(),
            std::thread::current()
                .name()
                .unwrap_or("test")
                .replace(":", "_")
        ));
        let _ = fs::remove_file(&path);
        path
    }

    #[test]
    fn one_time_token_redeems_once_then_is_invalid() {
        let store = EnrollmentTokenStore::new();
        let minted = store.mint(true, DEFAULT_INVITE_TTL);
        let now = now_unix();
        assert_eq!(
            store.redeem(&minted.token, now),
            RedeemOutcome::EnrollOk { one_time: true }
        );
        assert_eq!(store.redeem(&minted.token, now), RedeemOutcome::Invalid);
    }

    #[test]
    fn one_time_token_is_consumed_by_a_failed_redeem() {
        let store = EnrollmentTokenStore::new();
        let minted = store.mint(true, Duration::from_secs(0));
        // First redeem lands exactly at/past expiry: rejected, but the token
        // is consumed anyway (fail-closed).
        assert_eq!(
            store.redeem(&minted.token, now_unix()),
            RedeemOutcome::Expired
        );
        assert_eq!(
            store.redeem(&minted.token, now_unix()),
            RedeemOutcome::Invalid
        );
    }

    #[test]
    fn deploy_token_survives_redemption_until_expiry() {
        let store = EnrollmentTokenStore::new();
        let minted = store.mint(false, DEFAULT_DEPLOY_TTL);
        let now = now_unix();
        for _ in 0..3 {
            assert_eq!(
                store.redeem(&minted.token, now),
                RedeemOutcome::EnrollOk { one_time: false }
            );
        }
        // Past expiry the deploy token reports Expired, and a later attempt
        // keeps reporting Expired only while the record was swept — after
        // the sweep the outcome degrades to Invalid, both rejections.
        let past = minted.expires_at_unix + 1;
        assert_eq!(store.redeem(&minted.token, past), RedeemOutcome::Expired);
        assert_eq!(store.redeem(&minted.token, past), RedeemOutcome::Invalid);
    }

    #[test]
    fn expiry_boundary_is_expires_at_minus_one_ok() {
        let store = EnrollmentTokenStore::new();
        let minted = store.mint(true, DEFAULT_INVITE_TTL);
        assert_eq!(
            store.redeem(&minted.token, minted.expires_at_unix - 1),
            RedeemOutcome::EnrollOk { one_time: true }
        );
    }

    #[test]
    fn unknown_token_is_invalid() {
        let store = EnrollmentTokenStore::new();
        assert_eq!(
            store.redeem("not-a-real-token", now_unix()),
            RedeemOutcome::Invalid
        );
    }

    #[test]
    fn persist_load_round_trip_keeps_live_tokens() {
        let path = temp_path("round-trip.json");
        let store = EnrollmentTokenStore::new();
        let live = store.mint(true, DEFAULT_INVITE_TTL);
        let deploy = store.mint(false, DEFAULT_DEPLOY_TTL);
        store.persist(&path).expect("persist should succeed");

        let loaded = EnrollmentTokenStore::load(&path).expect("load should succeed");
        let now = now_unix();
        assert_eq!(
            loaded.redeem(&live.token, now),
            RedeemOutcome::EnrollOk { one_time: true }
        );
        assert_eq!(
            loaded.redeem(&deploy.token, now),
            RedeemOutcome::EnrollOk { one_time: false }
        );

        crate::infra::best_effort::remove_file(&path);
    }

    #[test]
    fn persist_sweeps_expired_tokens() {
        let path = temp_path("sweep.json");
        let store = EnrollmentTokenStore::new();
        let live = store.mint(true, DEFAULT_INVITE_TTL);
        let expired = store.mint(false, Duration::from_secs(0));
        assert_eq!(
            store.redeem(&expired.token, now_unix()),
            RedeemOutcome::Expired
        );
        store.mint(false, Duration::from_secs(0));
        store.persist(&path).expect("persist should succeed");

        let loaded = EnrollmentTokenStore::load(&path).expect("load should succeed");
        assert_eq!(
            loaded.redeem(&live.token, now_unix()),
            RedeemOutcome::EnrollOk { one_time: true }
        );
        assert_eq!(
            loaded.redeem(&expired.token, now_unix()),
            RedeemOutcome::Invalid
        );

        crate::infra::best_effort::remove_file(&path);
    }

    #[test]
    fn load_missing_file_yields_empty_store() {
        let path = temp_path("missing.json");
        let loaded = EnrollmentTokenStore::load(&path).expect("missing file loads empty");
        assert_eq!(
            loaded.redeem("anything", now_unix()),
            RedeemOutcome::Invalid
        );
    }

    #[test]
    fn whitelist_add_and_remove_round_trip() {
        let dir = temp_path("whitelist-dir");
        let _ = fs::remove_dir_all(&dir);
        add_authorized_node(&dir, "ABCDEF0123", "pem-bytes").expect("add should succeed");
        assert_eq!(
            fs::read_to_string(dir.join("abcdef0123")).expect("entry should exist"),
            "pem-bytes"
        );
        assert!(remove_authorized_node(&dir, "AbCdEf0123").expect("remove should succeed"));
        assert!(!remove_authorized_node(&dir, "abcdef0123").expect("second remove ok"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn enrollment_frame_flow_whitelists_and_responds() {
        let dir = temp_path("enroll-dir");
        let _ = fs::remove_dir_all(&dir);
        let store = EnrollmentTokenStore::new();
        let minted = store.mint(true, DEFAULT_INVITE_TTL);

        let response = handle_enrollment_frame(
            Frame::Enroll {
                token: minted.token.clone(),
            },
            "DeadBeef",
            "pem-bytes",
            &store,
            &dir,
            "relay-fp",
            now_unix(),
        );
        assert_eq!(
            response,
            Some(Frame::EnrollResponse {
                fingerprint: "relay-fp".to_string()
            })
        );
        assert!(dir.join("deadbeef").is_file(), "peer must be whitelisted");

        // The one-time token is gone now.
        let again = handle_enrollment_frame(
            Frame::Enroll {
                token: minted.token.clone(),
            },
            "DeadBeef",
            "pem-bytes",
            &store,
            &dir,
            "relay-fp",
            now_unix(),
        );
        assert!(
            matches!(
                again,
                Some(Frame::Error {
                    code: error_code::TOKEN_INVALID,
                    ..
                })
            ),
            "reused token must be rejected, got {again:?}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn enrollment_frame_flow_reports_expired() {
        let dir = temp_path("enroll-expired-dir");
        let _ = fs::remove_dir_all(&dir);
        let store = EnrollmentTokenStore::new();
        let minted = store.mint(true, Duration::from_secs(0));

        let response = handle_enrollment_frame(
            Frame::Enroll {
                token: minted.token,
            },
            "DeadBeef",
            "pem-bytes",
            &store,
            &dir,
            "relay-fp",
            now_unix() + 60,
        );
        assert!(matches!(
            response,
            Some(Frame::Error {
                code: error_code::TOKEN_EXPIRED,
                ..
            })
        ));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn enrollment_frame_rejects_non_enroll_first_frame() {
        let dir = temp_path("enroll-non-enroll-dir");
        let store = EnrollmentTokenStore::new();
        let response = handle_enrollment_frame(
            Frame::Heartbeat,
            "DeadBeef",
            "pem-bytes",
            &store,
            &dir,
            "relay-fp",
            now_unix(),
        );
        assert!(matches!(
            response,
            Some(Frame::Error {
                code: error_code::TOKEN_INVALID,
                ..
            })
        ));
    }

    #[test]
    fn pem_encode_wraps_base64_at_64_columns() {
        let der = vec![0xabu8; 120];
        let pem = pem_encode_cert(&der);
        assert!(pem.starts_with("-----BEGIN CERTIFICATE-----\n"));
        assert!(pem.ends_with("-----END CERTIFICATE-----\n"));
        let body: Vec<&str> = pem.lines().skip(1).take(pem.lines().count() - 2).collect();
        assert!(body.iter().all(|line| line.len() <= 64));
        assert_eq!(body.len(), 3, "160 base64 chars wrap into 3 lines");
    }
}
