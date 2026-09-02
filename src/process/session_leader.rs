//! Process session leader utilities.

use std::io;
use std::process::{Child, Command};

/// Spawn `command` as a detached child process so it survives the parent's
/// terminal/session exit.
pub fn spawn_session_leader(command: &mut Command) -> io::Result<Child> {
    crate::platform::process::spawn_detached(command)
}
