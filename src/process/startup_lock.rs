//! Process-level startup lock.

pub use crate::platform::file_lock::StartupLock;

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
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
