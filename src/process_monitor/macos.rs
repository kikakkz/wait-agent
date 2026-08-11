use crate::lifecycle::LifecycleError;
use crate::process_monitor::event::ProcessEvent;
use std::os::fd::{AsRawFd, RawFd};

/// Stub macOS process event source.
///
/// The real implementation should use kqueue EVFILT_PROC plus libproc scans.
/// This stub compiles on macOS but produces no events.
pub(crate) struct MacOsProcessEventSource;

impl MacOsProcessEventSource {
    pub fn new() -> Result<Self, LifecycleError> {
        Ok(Self)
    }

    pub fn read_events(&self, _out: &mut Vec<ProcessEvent>) -> Result<usize, LifecycleError> {
        Ok(0)
    }
}

impl AsRawFd for MacOsProcessEventSource {
    fn as_raw_fd(&self) -> RawFd {
        -1
    }
}
