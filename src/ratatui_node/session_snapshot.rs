//! Snapshot of a single local or remote PTY session's visible screen.

/// Plain/styled screen lines plus the cursor position and visibility state
/// reported by the underlying terminal emulator.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct SessionSnapshot {
    pub lines: Vec<String>,
    pub styled_lines: Vec<String>,
    pub cursor: Option<(u16, u16)>,
    pub cursor_visible: bool,
}
