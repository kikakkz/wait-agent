use std::fs;
use std::io;
use std::os::unix::io::AsRawFd;
use std::path::Path;

#[cfg(test)]
use std::path::PathBuf;

/// A process-level startup lock backed by `libc::flock` on a file.
///
/// The lock is released when the `StartupLock` value is dropped (or the process
/// exits), which is the normal `flock` behavior.
pub struct StartupLock {
    _file: fs::File,
}

impl StartupLock {
    /// Try to acquire an exclusive non-blocking lock.
    ///
    /// Returns `Ok(Some(lock))` on success, `Ok(None)` if another process holds
    /// the lock, and `Err` for I/O failures.
    pub fn try_acquire(path: &Path) -> io::Result<Option<Self>> {
        let file = open_lock_file(path)?;
        match flock(&file, libc::LOCK_EX | libc::LOCK_NB) {
            Ok(()) => Ok(Some(Self { _file: file })),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Acquire an exclusive blocking lock.
    pub fn acquire(path: &Path) -> io::Result<Self> {
        let file = open_lock_file(path)?;
        flock(&file, libc::LOCK_EX)?;
        Ok(Self { _file: file })
    }
}

fn open_lock_file(path: &Path) -> io::Result<fs::File> {
    fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
}

fn flock(file: &fs::File, operation: libc::c_int) -> io::Result<()> {
    // SAFETY: `file` is a valid, open `fs::File`, so `as_raw_fd()` returns a
    // valid file descriptor. `libc::flock` is async-signal-safe and only
    // operates on the open file description associated with that descriptor.
    let result = unsafe { libc::flock(file.as_raw_fd(), operation) };
    if result == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_lock_path() -> PathBuf {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_micros())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("waitagent-startup-lock-test-{timestamp}.lock"))
    }

    #[test]
    fn startup_lock_acquire_and_release() {
        let path = unique_lock_path();
        let lock = StartupLock::acquire(&path).expect("acquire should succeed");
        drop(lock);

        let lock = StartupLock::try_acquire(&path).expect("try_acquire should succeed");
        assert!(lock.is_some(), "lock should be available after release");
    }

    #[test]
    fn startup_lock_blocks_concurrent_non_blocking_acquire() {
        let path = unique_lock_path();
        let first = StartupLock::acquire(&path).expect("first acquire should succeed");

        let second = StartupLock::try_acquire(&path).expect("second try should not error");
        assert!(
            second.is_none(),
            "concurrent non-blocking acquire should be denied"
        );

        drop(first);
    }
}
