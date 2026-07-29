//! Compatibility shim: re-export the ratatui node server modules from their
//! new `ratatui_node` directory so existing call sites keep working.
pub use crate::runtime::ratatui_node::*;
