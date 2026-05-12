use std::fmt;
use std::io;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalSize {
    pub rows: u16,
    pub cols: u16,
    pub pixel_width: u16,
    pub pixel_height: u16,
}

impl Default for TerminalSize {
    fn default() -> Self {
        Self {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalSnapshot {
    pub input_is_tty: bool,
    pub output_is_tty: bool,
    pub size: TerminalSize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenSnapshot {
    pub size: TerminalSize,
    pub lines: Vec<String>,
    pub styled_lines: Vec<String>,
    pub active_style_ansi: String,
    pub scrollback: Vec<String>,
    pub styled_scrollback: Vec<String>,
    pub scroll_top: u16,
    pub scroll_bottom: u16,
    pub window_title: Option<String>,
    pub cursor_row: u16,
    pub cursor_col: u16,
    pub cursor_visible: bool,
    pub alternate_screen: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenState {
    pub normal: ScreenSnapshot,
    pub alternate: ScreenSnapshot,
    pub alternate_screen_active: bool,
    pub application_cursor_keys: bool,
}

impl ScreenState {
    pub fn active_snapshot(&self) -> &ScreenSnapshot {
        if self.alternate_screen_active {
            &self.alternate
        } else {
            &self.normal
        }
    }
}

#[derive(Debug)]
pub enum TerminalError {
    Io(String, io::Error),
    NotTty(String),
}

impl fmt::Display for TerminalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(context, error) => write!(f, "{context}: {error}"),
            Self::NotTty(name) => write!(f, "{name} is not attached to a terminal"),
        }
    }
}

impl std::error::Error for TerminalError {}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ScreenCell {
    pub(crate) ch: char,
    pub(crate) style: TextStyle,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct TextStyle {
    pub(crate) bold: bool,
    pub(crate) italic: bool,
    pub(crate) underline: bool,
    pub(crate) inverse: bool,
    pub(crate) foreground: Option<ColorValue>,
    pub(crate) background: Option<ColorValue>,
}

impl TextStyle {
    pub(crate) fn to_ansi(self) -> String {
        if self == Self::default() {
            return "\x1b[0m".to_string();
        }

        let mut params = vec!["0".to_string()];
        if self.bold {
            params.push("1".to_string());
        }
        if self.italic {
            params.push("3".to_string());
        }
        if self.underline {
            params.push("4".to_string());
        }
        if self.inverse {
            params.push("7".to_string());
        }
        if let Some(foreground) = self.foreground {
            foreground.push_ansi_params(&mut params, true);
        }
        if let Some(background) = self.background {
            background.push_ansi_params(&mut params, false);
        }

        format!("\x1b[{}m", params.join(";"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ColorValue {
    Indexed(u8),
    Rgb(u8, u8, u8),
}

impl ColorValue {
    fn push_ansi_params(self, params: &mut Vec<String>, foreground: bool) {
        let prefix = if foreground { "38" } else { "48" };
        match self {
            Self::Indexed(index) => {
                params.push(prefix.to_string());
                params.push("5".to_string());
                params.push(index.to_string());
            }
            Self::Rgb(red, green, blue) => {
                params.push(prefix.to_string());
                params.push("2".to_string());
                params.push(red.to_string());
                params.push(green.to_string());
                params.push(blue.to_string());
            }
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct SavedCursorState {
    pub(crate) row: u16,
    pub(crate) col: u16,
    pub(crate) style: TextStyle,
    pub(crate) valid: bool,
}

pub(crate) const WIDE_CONTINUATION: char = '\0';
