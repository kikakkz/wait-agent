use crossterm::event::{
    KeyCode as CrosstermKeyCode, KeyEvent, KeyModifiers as CrosstermKeyModifiers,
};

/// A serializable logical key event sent from the TUI to the node server.
///
/// The TUI no longer translates keys into ANSI bytes; it only describes which
/// key was pressed. The node server owns the terminal emulator and therefore
/// knows the active session's mode, so it performs the final byte translation.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LogicalKey {
    pub code: KeyCode,
    pub modifiers: KeyModifiers,
}

impl LogicalKey {
    /// Serialize to a compact JSON string suitable for the line protocol.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

impl From<&KeyEvent> for LogicalKey {
    fn from(event: &KeyEvent) -> Self {
        Self {
            code: KeyCode::from(event.code),
            modifiers: KeyModifiers::from(event.modifiers),
        }
    }
}

/// Platform-independent key code.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", content = "value")]
pub enum KeyCode {
    /// A Unicode character.
    Char(char),
    /// Function key F1..F24.
    F(u8),
    /// Arrow and editing keys.
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    /// Keys with fixed byte mappings.
    Enter,
    Backspace,
    Tab,
    BackTab,
    Esc,
    Null,
    /// A key we do not yet map. Carrying a label makes debugging easier.
    Unsupported(String),
}

impl From<CrosstermKeyCode> for KeyCode {
    fn from(code: CrosstermKeyCode) -> Self {
        match code {
            CrosstermKeyCode::Backspace => Self::Backspace,
            CrosstermKeyCode::Enter => Self::Enter,
            CrosstermKeyCode::Left => Self::Left,
            CrosstermKeyCode::Right => Self::Right,
            CrosstermKeyCode::Up => Self::Up,
            CrosstermKeyCode::Down => Self::Down,
            CrosstermKeyCode::Home => Self::Home,
            CrosstermKeyCode::End => Self::End,
            CrosstermKeyCode::PageUp => Self::PageUp,
            CrosstermKeyCode::PageDown => Self::PageDown,
            CrosstermKeyCode::Tab => Self::Tab,
            CrosstermKeyCode::BackTab => Self::BackTab,
            CrosstermKeyCode::Delete => Self::Delete,
            CrosstermKeyCode::Insert => Self::Insert,
            CrosstermKeyCode::F(n) => Self::F(n),
            CrosstermKeyCode::Char(c) => Self::Char(c),
            CrosstermKeyCode::Null => Self::Null,
            CrosstermKeyCode::Esc => Self::Esc,
            CrosstermKeyCode::CapsLock => Self::Unsupported("CapsLock".into()),
            CrosstermKeyCode::ScrollLock => Self::Unsupported("ScrollLock".into()),
            CrosstermKeyCode::NumLock => Self::Unsupported("NumLock".into()),
            CrosstermKeyCode::PrintScreen => Self::Unsupported("PrintScreen".into()),
            CrosstermKeyCode::Pause => Self::Unsupported("Pause".into()),
            CrosstermKeyCode::Menu => Self::Unsupported("Menu".into()),
            CrosstermKeyCode::KeypadBegin => Self::Unsupported("KeypadBegin".into()),
            CrosstermKeyCode::Media(key) => Self::Unsupported(format!("Media:{key:?}")),
            CrosstermKeyCode::Modifier(key) => Self::Unsupported(format!("Modifier:{key:?}")),
        }
    }
}

/// Modifier state carried with a logical key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct KeyModifiers {
    pub shift: bool,
    pub control: bool,
    pub alt: bool,
    #[serde(rename = "super")]
    pub super_key: bool,
    pub hyper: bool,
    pub meta: bool,
}

impl From<CrosstermKeyModifiers> for KeyModifiers {
    fn from(modifiers: CrosstermKeyModifiers) -> Self {
        Self {
            shift: modifiers.contains(CrosstermKeyModifiers::SHIFT),
            control: modifiers.contains(CrosstermKeyModifiers::CONTROL),
            alt: modifiers.contains(CrosstermKeyModifiers::ALT),
            super_key: modifiers.contains(CrosstermKeyModifiers::SUPER),
            hyper: modifiers.contains(CrosstermKeyModifiers::HYPER),
            meta: modifiers.contains(CrosstermKeyModifiers::META),
        }
    }
}
