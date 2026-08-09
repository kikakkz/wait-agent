use crate::lifecycle::LifecycleError;
use crate::process_monitor::event::ProcessEvent;
use std::sync::mpsc;

/// Stub macOS process event source.
///
/// The real implementation should use kqueue EVFILT_PROC plus libproc scans.
/// This stub compiles on macOS but produces no events.
pub(crate) struct MacOsProcessEventSource;

impl MacOsProcessEventSource {
    pub fn start(_tx: mpsc::Sender<ProcessEvent>) -> Result<Self, LifecycleError> {
        Ok(Self)
    }
}
