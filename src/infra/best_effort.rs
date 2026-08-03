//! Best-effort helpers that deliberately ignore operation results.
//!
//! These are used for cleanup and event-notification paths where failure is
//! unactionable: the socket may already be gone, the receiver may have dropped
//! during shutdown, or the operation is only a courtesy wakeup. Centralising
//! the discard here makes the intent explicit and keeps the call sites free of
//! repetitive `let _ =` boilerplate.

use std::fs;
use std::path::Path;
use std::sync::mpsc;

/// Remove a file if it exists; ignore errors.
///
/// Useful for stale socket files and pidfiles where absence is the desired
/// end state and an error (permission denied, busy) cannot be recovered.
pub fn remove_file(path: impl AsRef<Path>) {
    let _ = fs::remove_file(path);
}

/// Create a directory and its parents; ignore errors.
pub fn create_dir_all(path: impl AsRef<Path>) {
    let _ = fs::create_dir_all(path);
}

/// Remove a directory tree; ignore errors.
#[allow(dead_code)]
pub fn remove_dir_all(path: impl AsRef<Path>) {
    let _ = fs::remove_dir_all(path);
}

/// Write bytes to a file; ignore errors.
pub fn write_file(path: impl AsRef<Path>, contents: impl AsRef<[u8]>) {
    let _ = fs::write(path, contents);
}

/// Send a message on an `std::sync::mpsc` channel without blocking on a closed
/// receiver.
///
/// The receiver may be dropped because the runtime is shutting down; the event
/// is not actionable in that case.
#[allow(dead_code)]
pub fn send<T>(tx: &mpsc::Sender<T>, msg: T) {
    let _ = tx.send(msg);
}

/// Wait for a child process; ignore errors.
pub fn wait_child(child: std::process::Child) {
    let mut child = child;
    let _ = child.wait();
}

/// Shut down a Unix stream; ignore errors.
pub fn shutdown_stream(stream: &std::os::unix::net::UnixStream) {
    let _ = stream.shutdown(std::net::Shutdown::Both);
}
