//! Cross-platform pseudo-terminal (PTY) abstraction.
//!
//! Stage 4 starts by centralising the Unix PTY operations used by
//! `RatatuiAuthorityHostSession` and `AuthorityHostIoLoop`.  Windows
//! implementations will be added in a later slice.

use std::fs::File;

/// A PTY master/slave pair.
pub struct PtyPair {
    pub master: File,
    pub slave: File,
}

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub use unix::{openpty, resize, set_nonblocking};
#[cfg(windows)]
pub use windows::{openpty, resize, spawn_shell, ConPty, ConPtyChild};
