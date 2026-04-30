use crate::domain::session_catalog::ManagedSessionRecord;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ChromeSurfaceSize {
    pub width: usize,
    pub height: usize,
}

impl ChromeSurfaceSize {
    pub fn new(width: usize, height: usize) -> Self {
        Self { width, height }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidebarViewModel {
    pub active_socket: String,
    pub active_session: String,
    pub active_target: Option<String>,
    pub selected_target: Option<String>,
    pub sessions: Vec<ManagedSessionRecord>,
    pub surface: ChromeSurfaceSize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FooterViewModel {
    pub active_socket: String,
    pub active_session: String,
    pub active_target: Option<String>,
    pub sessions: Vec<ManagedSessionRecord>,
    pub listener_display: Option<String>,
    pub width: usize,
    pub fullscreen: bool,
}
