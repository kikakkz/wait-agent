//! In-memory auth stores (issue #131 v3): magic-token jtis (one-time,
//! 10-minute first use), session jtis (`last_seen` heartbeat semantics with
//! a 90-second idle eviction, 12-hour absolute expiry), and the magic-link
//! rate limiter (3 per 10 minutes per IP). All three are plain mutex-guarded
//! maps whose guards are leaf locks: every method takes its lock, does its
//! work, and drops the guard before returning — no lock is ever held across
//! an `.await`, and no other lock is acquired while holding one. A process
//! restart clears everything, which v3 explicitly accepts.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::web::auth::fingerprint::Probe;

/// A session with no heartbeat or authenticated request for this long is
/// evicted (v3: the JWT may still verify, but the jti is gone — dead).
pub const SESSION_IDLE_TIMEOUT: Duration = Duration::from_secs(90);

/// The magic-link rate limiter window (v3: 3 attempts per 10 minutes per IP).
const MAGIC_RATE_WINDOW: Duration = Duration::from_secs(10 * 60);
const MAGIC_RATE_LIMIT: u32 = 3;

struct MagicRecord {
    fp: String,
    probe: Probe,
    expires_at: Instant,
}

struct SessionRecord {
    fp: String,
    probe: Probe,
    last_seen: Instant,
    expires_at: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RateRecord {
    count: u32,
    window_started: Instant,
}

/// The three stores behind one struct so the routes layer holds a single
/// handle. Lock order: `magic`, `sessions`, and `limits` are independent
/// leaf locks — never nested.
pub struct AuthStores {
    magic: Mutex<HashMap<String, MagicRecord>>,
    sessions: Mutex<HashMap<String, SessionRecord>>,
    limits: Mutex<HashMap<IpAddr, RateRecord>>,
}

impl Default for AuthStores {
    fn default() -> Self {
        Self::new()
    }
}

impl AuthStores {
    pub fn new() -> Self {
        Self {
            magic: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
            limits: Mutex::new(HashMap::new()),
        }
    }

    /// Registers a freshly minted magic token; `ttl` bounds its first use.
    pub fn mint_magic(&self, jti: &str, fp: String, probe: Probe, ttl: Duration) {
        self.magic
            .lock()
            .expect("auth magic store lock poisoned")
            .insert(
                jti.to_string(),
                MagicRecord {
                    fp,
                    probe,
                    expires_at: Instant::now() + ttl,
                },
            );
    }

    /// Redeems a magic token. Consumed by ANY attempt, including rejected
    /// ones (fail-closed, same policy as enrollment tokens): a leaked token
    /// cannot be replayed after a clock reset. `Probe` rides along so the
    /// session inherits the exact signal set the fp was computed from.
    pub fn redeem_magic(&self, jti: &str) -> MagicRedeem {
        let mut magic = self.magic.lock().expect("auth magic store lock poisoned");
        // Sweep expired entries opportunistically so the map stays small.
        let now = Instant::now();
        magic.retain(|_, record| record.expires_at > now);
        let Some(record) = magic.remove(jti) else {
            return MagicRedeem::UnknownOrUsed;
        };
        if record.expires_at <= now {
            return MagicRedeem::Expired;
        }
        MagicRedeem::Ok {
            fp: record.fp,
            probe: record.probe,
        }
    }

    /// Registers a freshly minted session.
    pub fn mint_session(&self, jti: &str, fp: String, probe: Probe, ttl: Duration) {
        let now = Instant::now();
        self.sessions
            .lock()
            .expect("auth session store lock poisoned")
            .insert(
                jti.to_string(),
                SessionRecord {
                    fp,
                    probe,
                    last_seen: now,
                    expires_at: now + ttl,
                },
            );
    }

    /// Heartbeat / authenticated-request touch. Evicts the session when its
    /// absolute expiry passed or it idled past [`SESSION_IDLE_TIMEOUT`];
    /// otherwise refreshes `last_seen` and returns the stored fingerprint
    /// data for the caller's constant-time comparison.
    pub fn touch_session(&self, jti: &str) -> SessionTouch {
        let now = Instant::now();
        let mut sessions = self
            .sessions
            .lock()
            .expect("auth session store lock poisoned");
        let Some(record) = sessions.get_mut(jti) else {
            return SessionTouch::Unknown;
        };
        if record.expires_at <= now || record.last_seen + SESSION_IDLE_TIMEOUT <= now {
            sessions.remove(jti);
            return SessionTouch::Evicted;
        }
        record.last_seen = now;
        SessionTouch::Active {
            fp: record.fp.clone(),
            probe: record.probe.clone(),
        }
    }

    /// Removes a session (logout semantics; unused by the slice's routes but
    /// the eviction paths share it).
    #[allow(dead_code)]
    pub fn remove_session(&self, jti: &str) -> bool {
        self.sessions
            .lock()
            .expect("auth session store lock poisoned")
            .remove(jti)
            .is_some()
    }

    /// Whether another magic-link request from `ip` is allowed now (v3:
    /// 3 per 10 minutes per IP).
    pub fn allow_magic_request(&self, ip: IpAddr) -> bool {
        let now = Instant::now();
        let mut limits = self.limits.lock().expect("auth rate limit lock poisoned");
        let record = limits.entry(ip).or_insert(RateRecord {
            count: 0,
            window_started: now,
        });
        if now.duration_since(record.window_started) >= MAGIC_RATE_WINDOW {
            record.count = 0;
            record.window_started = now;
        }
        if record.count >= MAGIC_RATE_LIMIT {
            return false;
        }
        record.count += 1;
        true
    }
}

/// Outcome of [`AuthStores::redeem_magic`].
#[derive(Debug)]
pub enum MagicRedeem {
    /// The token was live; carries its fingerprint data for the caller's
    /// comparison, and the token is now consumed.
    Ok { fp: String, probe: Probe },
    /// Unknown jti or already consumed.
    UnknownOrUsed,
    /// Known but past its first-use TTL.
    Expired,
}

/// Outcome of [`AuthStores::touch_session`].
#[derive(Debug)]
pub enum SessionTouch {
    /// Live: `last_seen` refreshed; carries the stored fingerprint data.
    Active { fp: String, probe: Probe },
    /// Unknown jti (never existed, evicted, or this process restarted).
    Unknown,
    /// Idle past the heartbeat timeout or past the absolute expiry.
    Evicted,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe(tag: &str) -> Probe {
        Probe {
            platform: format!("platform-{tag}"),
            timezone: "UTC".to_string(),
            language: "en".to_string(),
            screen: "800x600".to_string(),
        }
    }

    #[test]
    fn magic_tokens_are_one_time_and_expire() {
        let stores = AuthStores::new();
        stores.mint_magic(
            "jti-1",
            "fp".to_string(),
            probe("a"),
            Duration::from_secs(60),
        );
        match stores.redeem_magic("jti-1") {
            MagicRedeem::Ok { fp, probe } => {
                assert_eq!(fp, "fp");
                assert_eq!(probe.platform, "platform-a");
            }
            other => panic!("first redeem should succeed, got {other:?}"),
        }
        assert!(
            matches!(stores.redeem_magic("jti-1"), MagicRedeem::UnknownOrUsed),
            "a magic token redeems exactly once"
        );

        stores.mint_magic(
            "jti-2",
            "fp".to_string(),
            probe("b"),
            Duration::from_millis(0),
        );
        std::thread::sleep(Duration::from_millis(2));
        // The opportunistic sweep erases expired entries before lookup, so
        // an expired token is reported exactly like an unknown one; both
        // rejections, same policy as enrollment deploy tokens.
        assert!(
            matches!(stores.redeem_magic("jti-2"), MagicRedeem::UnknownOrUsed),
            "an expired token is swept then reports UnknownOrUsed"
        );
        assert!(matches!(
            stores.redeem_magic("never-existed"),
            MagicRedeem::UnknownOrUsed
        ));
    }

    #[test]
    fn sessions_idle_out_and_expire_absolutely() {
        let stores = AuthStores::new();
        stores.mint_session("s-1", "fp".to_string(), probe("x"), Duration::from_secs(60));
        match stores.touch_session("s-1") {
            SessionTouch::Active { fp, .. } => assert_eq!(fp, "fp"),
            other => panic!("fresh session should be active, got {other:?}"),
        }
        assert!(matches!(
            stores.touch_session("s-1"),
            SessionTouch::Active { .. }
        ));

        stores.mint_session("s-2", "fp".to_string(), probe("y"), Duration::from_secs(60));
        // Force idle eviction by rewriting last_seen into the past.
        {
            let mut sessions = stores.sessions.lock().expect("lock");
            let record = sessions.get_mut("s-2").expect("record");
            record.last_seen = Instant::now() - SESSION_IDLE_TIMEOUT;
        }
        assert!(
            matches!(stores.touch_session("s-2"), SessionTouch::Evicted),
            "idle past the heartbeat timeout evicts"
        );
        assert!(
            matches!(stores.touch_session("s-2"), SessionTouch::Unknown),
            "an evicted jti is gone for good"
        );

        stores.mint_session(
            "s-3",
            "fp".to_string(),
            probe("z"),
            Duration::from_millis(1),
        );
        std::thread::sleep(Duration::from_millis(3));
        assert!(
            matches!(stores.touch_session("s-3"), SessionTouch::Evicted),
            "absolute expiry evicts even with recent heartbeats"
        );
        assert!(matches!(
            stores.touch_session("missing"),
            SessionTouch::Unknown
        ));
    }

    #[test]
    fn magic_requests_are_limited_per_ip_per_window() {
        let stores = AuthStores::new();
        let ip: IpAddr = "192.0.2.1".parse().unwrap();
        let other: IpAddr = "192.0.2.2".parse().unwrap();
        assert!(stores.allow_magic_request(ip));
        assert!(stores.allow_magic_request(ip));
        assert!(stores.allow_magic_request(ip));
        assert!(
            !stores.allow_magic_request(ip),
            "the 4th request inside the window is refused"
        );
        assert!(stores.allow_magic_request(other), "the limiter is per IP");
    }
}
