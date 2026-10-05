//! Ed25519-signed JWTs for the WebUI auth (issue #131 v3): the magic token
//! (`sub = "magic"`, 10-minute first-use TTL) and the session token
//! (`sub = "session"`, 12-hour absolute lifetime) share one envelope —
//! `{header}.{payload}.{signature}` in base64url, Ed25519 signature over the
//! first two segments. Hand-rolled on the existing `ring` dependency (zero
//! new transitive crates): the JWT envelope is 20 lines and fully unit
//! tested, and `jsonwebtoken` would pull in the same primitives plus more.
//!
//! The server keypair is generated on first start at
//! `waitagent_home()/web-auth.key` (PKCS#8 PEM, 0600 on unix) and derived
//! from `rcgen`'s Ed25519 facility; the public half is re-derived on every
//! load, so the file is the single secret to back up (rotation command:
//! separate issue, per #131).

use std::fmt;
use std::fs;
use std::io;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use rand::RngCore;
use ring::rand::SystemRandom;
use ring::signature::{Ed25519KeyPair, KeyPair, UnparsedPublicKey, ED25519};
use serde::{Deserialize, Serialize};

/// First-use TTL of a magic token (v3 parameter table).
pub const MAGIC_TTL_SECS: u64 = 10 * 60;
/// Absolute lifetime of a session (v3 parameter table).
pub const SESSION_TTL_SECS: u64 = 12 * 60 * 60;

/// Fixed JWT header: Ed25519 signatures, nothing negotiable.
const JWT_HEADER: &str = r#"{"alg":"Ed25519","typ":"JWT"}"#;

/// The signed claim set shared by magic and session tokens.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Claims {
    /// `"magic"` or `"session"`.
    pub sub: String,
    /// 128-bit one-time identifier, hex-encoded; liveness lives in
    /// [`AuthStores`](crate::web::auth::store::AuthStores), not in the JWT.
    pub jti: String,
    /// Issued-at, unix seconds.
    pub iat: u64,
    /// Expiry, unix seconds (iat + the token's TTL).
    pub exp: u64,
    /// Device fingerprint (see `fingerprint`); constant-time-compared on
    /// every use.
    pub fp: String,
}

/// The Ed25519 server keypair used to sign and verify web tokens.
pub struct WebAuthKeys {
    key_pair: Ed25519KeyPair,
}

impl WebAuthKeys {
    /// Loads the keypair at `path`, generating and persisting it (0600 on
    /// unix, raw PKCS#8 DER) when absent.
    pub fn load_or_generate(path: &Path) -> Result<Self, AuthKeyError> {
        if path.is_file() {
            let pkcs8 = fs::read(path)?;
            let key_pair = Ed25519KeyPair::from_pkcs8(&pkcs8).map_err(|_| {
                AuthKeyError::Parse(format!("{} is not valid Ed25519 PKCS#8", path.display()))
            })?;
            return Ok(Self { key_pair });
        }
        let rng = SystemRandom::new();
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng)
            .map_err(|_| AuthKeyError::Generate("ring key generation failed".to_string()))?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        write_private_key(path, pkcs8.as_ref())?;
        Self::load_or_generate(path)
    }

    /// Signs `claims` into a compact JWT.
    pub fn sign(&self, claims: &Claims) -> Result<String, AuthKeyError> {
        let payload = serde_json::to_string(claims)
            .map_err(|error| AuthKeyError::Encode(error.to_string()))?;
        let signing_input = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(JWT_HEADER.as_bytes()),
            URL_SAFE_NO_PAD.encode(payload.as_bytes())
        );
        let signature = self.key_pair.sign(signing_input.as_bytes());
        Ok(format!(
            "{}.{}",
            signing_input,
            URL_SAFE_NO_PAD.encode(signature.as_ref())
        ))
    }

    /// Verifies a compact JWT: signature, `sub`, and expiry. Liveness and
    /// fingerprint checks belong to the caller (stores layer).
    pub fn verify(&self, token: &str, expected_sub: &str) -> Result<Claims, AuthKeyError> {
        let mut parts = token.split('.');
        let (Some(header), Some(payload), Some(signature), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(AuthKeyError::Malformed);
        };
        if header != URL_SAFE_NO_PAD.encode(JWT_HEADER.as_bytes()) {
            return Err(AuthKeyError::Malformed);
        }
        let signature = URL_SAFE_NO_PAD
            .decode(signature.as_bytes())
            .map_err(|_| AuthKeyError::Malformed)?;
        let public_key = UnparsedPublicKey::new(&ED25519, self.key_pair.public_key().as_ref());
        public_key
            .verify(format!("{header}.{payload}").as_bytes(), &signature)
            .map_err(|_| AuthKeyError::BadSignature)?;
        let claims: Claims = serde_json::from_slice(
            &URL_SAFE_NO_PAD
                .decode(payload.as_bytes())
                .map_err(|_| AuthKeyError::Malformed)?,
        )
        .map_err(|_| AuthKeyError::Malformed)?;
        if claims.sub != expected_sub {
            return Err(AuthKeyError::WrongSubject(claims.sub));
        }
        let now = now_unix();
        if claims.exp <= now {
            return Err(AuthKeyError::Expired);
        }
        Ok(claims)
    }
}

/// Mints a fresh 128-bit jti (hex); one-time semantics live in the stores.
pub fn new_jti() -> String {
    let mut bytes = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    let mut out = String::with_capacity(32);
    use std::fmt::Write;
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Unix seconds, saturating at 0 (the error_log idiom).
pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

/// Errors of the web auth keys and JWT envelope.
#[derive(Debug)]
pub enum AuthKeyError {
    Io(io::Error),
    Generate(String),
    Parse(String),
    Encode(String),
    Malformed,
    BadSignature,
    WrongSubject(String),
    Expired,
}

impl fmt::Display for AuthKeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "io error: {error}"),
            Self::Generate(message) => write!(f, "key generation failed: {message}"),
            Self::Parse(message) => write!(f, "key parse failed: {message}"),
            Self::Encode(message) => write!(f, "claims encode failed: {message}"),
            Self::Malformed => write!(f, "malformed token"),
            Self::BadSignature => write!(f, "bad token signature"),
            Self::WrongSubject(sub) => write!(f, "unexpected token subject {sub:?}"),
            Self::Expired => write!(f, "token expired"),
        }
    }
}

impl std::error::Error for AuthKeyError {}

impl From<io::Error> for AuthKeyError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

fn write_private_key(path: &Path, contents: &[u8]) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(contents)?;
        file.flush()?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        fs::write(path, contents)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_key_path(name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "waitagent-web-auth-{name}-{}-{}.key",
            std::process::id(),
            std::thread::current()
                .name()
                .unwrap_or("test")
                .replace(":", "_")
        ));
        let _ = fs::remove_file(&path);
        path
    }

    fn claims(sub: &str, fp: &str, iat: u64, ttl: u64) -> Claims {
        Claims {
            sub: sub.to_string(),
            jti: new_jti(),
            iat,
            exp: iat + ttl,
            fp: fp.to_string(),
        }
    }

    #[test]
    fn sign_verify_round_trip_and_regeneration_keeps_identity() {
        let path = temp_key_path("round-trip");
        let keys = WebAuthKeys::load_or_generate(&path).expect("generate");
        let token = keys
            .sign(&claims("magic", "fp-1", now_unix(), MAGIC_TTL_SECS))
            .expect("sign");
        let verified = keys.verify(&token, "magic").expect("verify");
        assert_eq!(verified.sub, "magic");
        assert_eq!(verified.fp, "fp-1");

        // A second process loading the same file sees the same keypair.
        let reloaded = WebAuthKeys::load_or_generate(&path).expect("reload");
        let verified = reloaded.verify(&token, "magic").expect("reloaded verifies");
        assert_eq!(verified.fp, "fp-1");
        crate::infra::best_effort::remove_file(&path);
    }

    #[cfg(unix)]
    #[test]
    fn key_file_has_600_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let path = temp_key_path("perms");
        WebAuthKeys::load_or_generate(&path).expect("generate");
        let mode = fs::metadata(&path).expect("metadata").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        crate::infra::best_effort::remove_file(&path);
    }

    #[test]
    fn tampered_signature_and_payload_are_rejected() {
        let path = temp_key_path("tamper");
        let keys = WebAuthKeys::load_or_generate(&path).expect("generate");
        let token = keys
            .sign(&claims("session", "fp", now_unix(), SESSION_TTL_SECS))
            .expect("sign");

        let mut parts = token.split('.').map(str::to_string).collect::<Vec<_>>();
        // Flip one character in the payload segment.
        let payload = URL_SAFE_NO_PAD.encode(
            b"{\"sub\":\"session\",\"jti\":\"x\",\"iat\":1,\"exp\":9999999999,\"fp\":\"EVIL\"}",
        );
        parts[1] = payload;
        let forged = parts.join(".");
        assert!(matches!(
            keys.verify(&forged, "session"),
            Err(AuthKeyError::BadSignature)
        ));

        let mut sig = token.rsplit('.').next().expect("signature").to_string();
        sig.replace_range(0..1, if sig.starts_with('A') { "B" } else { "A" });
        let bad_sig = format!("{}.{}", token.rsplit_once('.').expect("two").0, sig);
        assert!(matches!(
            keys.verify(&bad_sig, "session"),
            Err(AuthKeyError::BadSignature)
        ));
        crate::infra::best_effort::remove_file(&path);
    }

    #[test]
    fn wrong_subject_expired_and_malformed_are_rejected() {
        let path = temp_key_path("subjects");
        let keys = WebAuthKeys::load_or_generate(&path).expect("generate");
        let magic = keys
            .sign(&claims("magic", "fp", now_unix(), MAGIC_TTL_SECS))
            .expect("sign");
        assert!(matches!(
            keys.verify(&magic, "session"),
            Err(AuthKeyError::WrongSubject(_))
        ));

        let expired = keys
            .sign(&claims(
                "magic",
                "fp",
                now_unix() - 2 * MAGIC_TTL_SECS,
                MAGIC_TTL_SECS,
            ))
            .expect("sign");
        assert!(matches!(
            keys.verify(&expired, "magic"),
            Err(AuthKeyError::Expired)
        ));

        assert!(matches!(
            keys.verify("nope", "magic"),
            Err(AuthKeyError::Malformed)
        ));
        let mut extra = magic.clone();
        extra.push_str(".more");
        assert!(matches!(
            keys.verify(&extra, "magic"),
            Err(AuthKeyError::Malformed)
        ));
        crate::infra::best_effort::remove_file(&path);
    }

    #[test]
    fn jtis_are_unique_128_bit_hex() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..256 {
            let jti = new_jti();
            assert_eq!(jti.len(), 32);
            assert!(jti.chars().all(|ch| ch.is_ascii_hexdigit()));
            assert!(seen.insert(jti), "jtis must be unique");
        }
    }
}
