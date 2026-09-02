//! Platform abstraction layer for WaitAgent.
//!
//! This module isolates all OS-specific APIs (Unix Domain Sockets, PTYs,
//! signals, file locks, etc.) behind cross-platform traits. Business code
//! should only import from this module, never directly from
//! `std::os::unix`, `libc`, `rustix`, or `signal-hook`.

pub mod file_lock;
pub mod file_watcher;
pub mod local_ipc;
pub mod process;
pub mod pty;
pub mod remote_ipc;
pub mod signal;
pub mod wake_pipe;
