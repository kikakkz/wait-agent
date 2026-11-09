//! Coordination service: enrollment and operator administration over iroh (issue #236; see docs/coordination-design.md).

pub mod dashboard;
pub mod enrollment;
pub mod presence;
pub mod server;
pub mod toml_store;

/// ALPN for the coordination node surface (enroll-request, join).
pub const NODE_ALPN: &[u8] = b"waitagent/coordination/0";
/// ALPN for the operator admin surface (requests, approve, revoke).
pub const ADMIN_ALPN: &[u8] = b"waitagent/coordination-admin/0";

/// Default UDP listen port for `coordination serve`.
pub const DEFAULT_LISTEN_PORT: u16 = 7479;
/// Default join-token lifetime: 15 minutes.
pub const DEFAULT_TOKEN_TTL_SECS: u64 = 15 * 60;

/// Default storage root: `~/.waitagent/coordination/`.
pub fn default_home() -> std::path::PathBuf {
    crate::host::ssh::remote_host_home::waitagent_home().join("coordination")
}
