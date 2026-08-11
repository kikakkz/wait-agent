use crate::lifecycle::LifecycleError;
use crate::process_monitor::event::ProcessEvent;
use std::os::windows::io::{AsRawHandle, RawHandle};

/// Stub Windows process event source.
///
/// The real implementation should use ETW to subscribe to process start/stop
/// events. This stub compiles on Windows but produces no events.
pub(crate) struct WindowsProcessEventSource;

impl WindowsProcessEventSource {
    pub fn new() -> Result<Self, LifecycleError> {
        Ok(Self)
    }

    pub fn read_events(&self, _out: &mut Vec<ProcessEvent>) -> Result<usize, LifecycleError> {
        Ok(0)
    }
}

impl AsRawHandle for WindowsProcessEventSource {
    fn as_raw_handle(&self) -> RawHandle {
        std::ptr::null_mut()
    }
}
