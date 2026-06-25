use std::io;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread;

pub fn spawn_waitagent_sidecar(current_executable: &Path, args: Vec<String>) -> io::Result<()> {
    let child = spawn_waitagent_sidecar_child(current_executable, args)?;
    reap_waitagent_sidecar(child);
    Ok(())
}

pub fn spawn_waitagent_sidecar_child(
    current_executable: &Path,
    args: Vec<String>,
) -> io::Result<Child> {
    let mut command = Command::new(current_executable);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());

    #[cfg(unix)]
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }

    command.spawn()
}

pub fn reap_waitagent_sidecar(mut child: Child) {
    thread::spawn(move || {
        let _ = child.wait();
    });
}
