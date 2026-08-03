use crate::process::session_leader::spawn_session_leader;
use std::io;
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

    spawn_session_leader(&mut command)
}

pub fn reap_waitagent_sidecar(child: Child) {
    thread::spawn(move || {
        crate::infra::best_effort::wait_child(child);
    });
}
