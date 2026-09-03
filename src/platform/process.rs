//! Cross-platform detached child process creation.

use std::io;
use std::process::{Child, Command};

/// Spawn `command` as a detached child process so it survives the parent's
/// terminal/session exit.
#[cfg(unix)]
pub fn spawn_detached(command: &mut Command) -> io::Result<Child> {
    use std::os::unix::process::CommandExt;

    // SAFETY: `pre_exec` runs in the child between fork and exec. We only call
    // the async-signal-safe `libc::setsid` and propagate the error as the spawn
    // error if it fails.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command.spawn()
}

/// Windows: spawn the command in its own process group without a console so
/// it survives the parent's terminal exit.
#[cfg(windows)]
pub fn spawn_detached(command: &mut Command) -> io::Result<Child> {
    use std::os::windows::process::CommandExt;
    use std::process::Stdio;

    // A detached process cannot inherit any console handles. Force all stdio
    // to null so the spawn succeeds even if the caller left one as `inherit`.
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(
            windows_sys::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP
                | windows_sys::Win32::System::Threading::DETACHED_PROCESS,
        )
        .spawn()
}
