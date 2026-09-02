//! Cross-platform process-level startup lock.

use std::fs;
use std::io;
use std::path::Path;

/// A process-level startup lock backed by a file.
///
/// The lock is released when the value is dropped (or the process exits).
pub struct StartupLock {
    _file: fs::File,
}

impl StartupLock {
    /// Try to acquire an exclusive non-blocking lock.
    ///
    /// Returns `Ok(Some(lock))` on success, `Ok(None)` if another process holds
    /// the lock, and `Err` for I/O failures.
    #[cfg(unix)]
    pub fn try_acquire(path: &Path) -> io::Result<Option<Self>> {
        let file = open_lock_file(path)?;
        match flock(&file, libc::LOCK_EX | libc::LOCK_NB) {
            Ok(()) => Ok(Some(Self { _file: file })),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Acquire an exclusive blocking lock.
    #[cfg(unix)]
    pub fn acquire(path: &Path) -> io::Result<Self> {
        let file = open_lock_file(path)?;
        flock(&file, libc::LOCK_EX)?;
        Ok(Self { _file: file })
    }

    #[cfg(windows)]
    pub fn try_acquire(path: &Path) -> io::Result<Option<Self>> {
        let file = open_lock_file(path)?;
        match lock_file(&file, true) {
            Ok(()) => Ok(Some(Self { _file: file })),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(error) => Err(error),
        }
    }

    #[cfg(windows)]
    pub fn acquire(path: &Path) -> io::Result<Self> {
        let file = open_lock_file(path)?;
        lock_file(&file, false)?;
        Ok(Self { _file: file })
    }
}

#[cfg(windows)]
fn lock_file(file: &fs::File, non_blocking: bool) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{ERROR_LOCK_VIOLATION, HANDLE};
    use windows_sys::Win32::Storage::FileSystem::{
        LockFileEx, LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY,
    };
    use windows_sys::Win32::System::IO::OVERLAPPED;

    let handle = file.as_raw_handle() as HANDLE;
    let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
    let mut flags = LOCKFILE_EXCLUSIVE_LOCK;
    if non_blocking {
        flags |= LOCKFILE_FAIL_IMMEDIATELY;
    }

    // SAFETY: `handle` is a valid file handle and `overlapped` is zeroed.
    // Locking a single byte at offset 0 is sufficient for a process-level mutex.
    let result = unsafe { LockFileEx(handle, flags, 0, 1, 0, &mut overlapped) };
    if result == 0 {
        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(ERROR_LOCK_VIOLATION as i32) {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "file lock held by another process",
            ));
        }
        return Err(err);
    }
    Ok(())
}

fn open_lock_file(path: &Path) -> io::Result<fs::File> {
    fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
}

#[cfg(unix)]
fn flock(file: &fs::File, operation: libc::c_int) -> io::Result<()> {
    use std::os::unix::io::AsRawFd;

    // SAFETY: `file` is a valid open `fs::File`, so `as_raw_fd()` returns a
    // valid file descriptor. `libc::flock` only operates on that descriptor.
    let result = unsafe { libc::flock(file.as_raw_fd(), operation) };
    if result == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
