use std::io;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command};

/// Spawn `command` as a new session leader by calling `setsid` in a
/// `pre_exec` hook.
///
/// This detaches the child from the caller's controlling terminal so it
/// survives the parent process exiting.
pub fn spawn_session_leader(command: &mut Command) -> io::Result<Child> {
    // SAFETY: `pre_exec` is unsafe because it runs in the child process between
    // `fork` and `exec`. We only call the async-signal-safe `libc::setsid` and
    // return an `io::Error` if it fails, which `std` will propagate as the
    // child spawn error.
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
