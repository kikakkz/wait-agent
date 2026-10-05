//! Authentication for the WebUI (issue #131 slice 3, security design v3/v4):
//! magic-link login with Ed25519-signed JWTs and a device fingerprint that
//! binds every token to one machine/browser/network. All stores are
//! in-memory by design (v3: a restart invalidates every magic token and
//! session; nothing about logins is persisted).

pub mod fingerprint;
pub mod mail;
pub mod routes;
pub mod smtp;
pub mod store;
pub mod token;
