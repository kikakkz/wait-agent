//! Agent lifecycle signal server.
//!
//! Platform-specific implementation lives in `crate::platform::signal`; this
//! module preserves the existing public API.

pub use crate::platform::signal::AgentSignalServer;
