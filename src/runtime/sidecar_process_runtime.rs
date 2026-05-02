use std::io;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;

pub fn spawn_waitagent_sidecar(current_executable: &Path, args: Vec<String>) -> io::Result<()> {
    let mut command = Command::new(current_executable);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    #[cfg(unix)]
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let mut child = command.spawn()?;
    thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}
