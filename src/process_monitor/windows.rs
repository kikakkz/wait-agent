use crate::lifecycle::LifecycleError;
use crate::process_monitor::event::ProcessEvent;
use std::sync::mpsc;

/// Stub Windows process event source.
///
/// The real implementation should use ETW to subscribe to process start/stop
/// events. This stub compiles on Windows but produces no events.
pub(crate) struct WindowsProcessEventSource;

impl WindowsProcessEventSource {
    pub fn start(_tx: mpsc::Sender<ProcessEvent>) -> Result<Self, LifecycleError> {
        Ok(Self)
    }
}
