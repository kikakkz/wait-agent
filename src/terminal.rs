#![allow(dead_code)]

use crate::pty::PtySize;
use std::fmt;
use std::io::{self, Write};
use std::mem::MaybeUninit;
use std::os::raw::{c_int, c_uchar, c_uint, c_ulong};
use std::os::unix::io::RawFd;

const STDIN_FD: RawFd = 0;
const STDOUT_FD: RawFd = 1;
const TIOCGWINSZ: c_ulong = 0x5413;
const TCSAFLUSH: c_int = 2;
const NCCS: usize = 32;
const ENTER_ALTERNATE_SCREEN: &str = "\x1b[?1049h\x1b[H";
const LEAVE_ALTERNATE_SCREEN: &str = "\x1b[?1049l\x1b[?25h";

const BRKINT: c_uint = 0o000002;
const ICRNL: c_uint = 0o000400;
const INPCK: c_uint = 0o000020;
const ISTRIP: c_uint = 0o000040;
const IXON: c_uint = 0o002000;
const OPOST: c_uint = 0o000001;
const CS8: c_uint = 0o000060;
const ECHO: c_uint = 0o000010;
const ICANON: c_uint = 0o000002;
const IEXTEN: c_uint = 0o100000;
const ISIG: c_uint = 0o000001;
const VTIME: usize = 5;
const VMIN: usize = 6;

extern "C" {
    fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
    fn isatty(fd: c_int) -> c_int;
    fn tcgetattr(fd: c_int, termios_p: *mut Termios) -> c_int;
    fn tcsetattr(fd: c_int, optional_actions: c_int, termios_p: *const Termios) -> c_int;
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Winsize {
    ws_row: u16,
    ws_col: u16,
    ws_xpixel: u16,
    ws_ypixel: u16,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Termios {
    c_iflag: c_uint,
    c_oflag: c_uint,
    c_cflag: c_uint,
    c_lflag: c_uint,
    c_line: c_uchar,
    c_cc: [c_uchar; NCCS],
    c_ispeed: c_uint,
    c_ospeed: c_uint,
}

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

impl From<TerminalSize> for PtySize {
    fn from(value: TerminalSize) -> Self {
        Self {
            rows: value.rows,
            cols: value.cols,
            pixel_width: value.pixel_width,
            pixel_height: value.pixel_height,
        }
    }
}

impl From<PtySize> for TerminalSize {
    fn from(value: PtySize) -> Self {
        Self {
            rows: value.rows,
            cols: value.cols,
            pixel_width: value.pixel_width,
            pixel_height: value.pixel_height,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalSnapshot {
    pub input_is_tty: bool,
    pub output_is_tty: bool,
    pub size: TerminalSize,
}

#[derive(Debug, Default, Clone, Copy)]
struct ResizeTracker {
    last_size: Option<TerminalSize>,
}

impl ResizeTracker {
    fn observe(&mut self, size: TerminalSize) -> Option<TerminalSize> {
        if self.last_size == Some(size) {
            None
        } else {
            self.last_size = Some(size);
            Some(size)
        }
    }
}

#[derive(Debug)]
pub struct TerminalRuntime {
    input_fd: RawFd,
    output_fd: RawFd,
    resize: ResizeTracker,
}

impl TerminalRuntime {
    pub fn stdio() -> Self {
        Self {
            input_fd: STDIN_FD,
            output_fd: STDOUT_FD,
            resize: ResizeTracker::default(),
        }
    }

    pub fn input_is_tty(&self) -> bool {
        is_tty(self.input_fd)
    }

    pub fn output_is_tty(&self) -> bool {
        is_tty(self.output_fd)
    }

    pub fn snapshot(&mut self) -> Result<TerminalSnapshot, TerminalError> {
        let size = self.current_size_or_default();
        self.resize.observe(size);

        Ok(TerminalSnapshot {
            input_is_tty: self.input_is_tty(),
            output_is_tty: self.output_is_tty(),
            size,
        })
    }

    pub fn current_size(&self) -> Result<TerminalSize, TerminalError> {
        if !self.output_is_tty() {
            return Err(TerminalError::NotTty("stdout".to_string()));
        }

        let mut winsize = MaybeUninit::<Winsize>::uninit();
        let result = unsafe { ioctl(self.output_fd, TIOCGWINSZ, winsize.as_mut_ptr()) };
        if result != 0 {
            return Err(TerminalError::Io(
                "failed to query terminal size".to_string(),
                io::Error::last_os_error(),
            ));
        }

        let winsize = unsafe { winsize.assume_init() };
        Ok(TerminalSize {
            rows: winsize.ws_row,
            cols: winsize.ws_col,
            pixel_width: winsize.ws_xpixel,
            pixel_height: winsize.ws_ypixel,
        })
    }

    pub fn current_size_or_default(&self) -> TerminalSize {
        self.current_size().unwrap_or_default()
    }

    pub fn capture_resize(&mut self) -> Result<Option<TerminalSize>, TerminalError> {
        let size = self.current_size()?;
        Ok(self.resize.observe(size))
    }

    pub fn enter_raw_mode(&self) -> Result<RawModeGuard, TerminalError> {
        if !self.input_is_tty() {
            return Err(TerminalError::NotTty("stdin".to_string()));
        }

        let original = read_termios(self.input_fd)?;
        let raw = make_raw(original);
        write_termios(self.input_fd, &raw)?;

        Ok(RawModeGuard {
            fd: self.input_fd,
            original,
            active: true,
        })
    }

    pub fn enter_alternate_screen(&self) -> Result<AlternateScreenGuard, TerminalError> {
        if !self.output_is_tty() {
            return Err(TerminalError::NotTty("stdout".to_string()));
        }

        write_escape(self.output_fd, ENTER_ALTERNATE_SCREEN)?;
        Ok(AlternateScreenGuard {
            fd: self.output_fd,
            active: true,
        })
    }
}

pub struct RawModeGuard {
    fd: RawFd,
    original: Termios,
    active: bool,
}

impl RawModeGuard {
    pub fn restore(&mut self) -> Result<(), TerminalError> {
        if !self.active {
            return Ok(());
        }

        write_termios(self.fd, &self.original)?;
        self.active = false;
        Ok(())
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

pub struct AlternateScreenGuard {
    fd: RawFd,
    active: bool,
}

impl AlternateScreenGuard {
    pub fn restore(&mut self) -> Result<(), TerminalError> {
        if !self.active {
            return Ok(());
        }

        write_escape(self.fd, LEAVE_ALTERNATE_SCREEN)?;
        self.active = false;
        Ok(())
    }
}

impl Drop for AlternateScreenGuard {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenSnapshot {
    pub size: TerminalSize,
    pub lines: Vec<String>,
    pub styled_lines: Vec<String>,
    pub active_style_ansi: String,
    pub scrollback: Vec<String>,
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

#[derive(Debug, Clone)]
pub struct TerminalEngine {
    normal: ScreenBuffer,
    alternate: ScreenBuffer,
    alternate_screen_active: bool,
    application_cursor_keys: bool,
    cursor_visible: bool,
    window_title: Option<String>,
    pending_escape: Vec<u8>,
    pending_utf8: Vec<u8>,
}

impl TerminalEngine {
    pub fn new(size: TerminalSize) -> Self {
        Self {
            normal: ScreenBuffer::new(size),
            alternate: ScreenBuffer::new(size),
            alternate_screen_active: false,
            application_cursor_keys: false,
            cursor_visible: true,
            window_title: None,
            pending_escape: Vec::new(),
            pending_utf8: Vec::new(),
        }
    }

    pub fn resize(&mut self, size: TerminalSize) {
        self.normal.resize(size);
        self.alternate.resize(size);
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        let _ = self.feed_and_collect_replies(bytes);
    }

    pub fn feed_and_collect_replies(&mut self, bytes: &[u8]) -> Vec<u8> {
        let mut input = Vec::with_capacity(self.pending_escape.len() + bytes.len());
        input.extend_from_slice(&self.pending_escape);
        input.extend_from_slice(bytes);
        self.pending_escape.clear();

        let mut plain = Vec::new();
        let mut replies = Vec::new();
        let mut index = 0;

        while index < input.len() {
            match input[index] {
                0x1b => {
                    self.flush_plain(&mut plain);
                    let escape_start = index;
                    index += 1;
                    if index >= input.len() {
                        self.pending_escape
                            .extend_from_slice(&input[escape_start..]);
                        break;
                    }
                    match input[index] {
                        b'[' => {
                            index += 1;
                            match self.consume_csi(&input, index, &mut replies) {
                                Some(next_index) => index = next_index,
                                None => {
                                    self.pending_escape
                                        .extend_from_slice(&input[escape_start..]);
                                    break;
                                }
                            }
                        }
                        b']' => {
                            index += 1;
                            match self.consume_osc(&input, index, &mut replies) {
                                Some(next_index) => index = next_index,
                                None => {
                                    self.pending_escape
                                        .extend_from_slice(&input[escape_start..]);
                                    break;
                                }
                            }
                        }
                        b'M' => {
                            self.active_buffer_mut().reverse_index();
                            index += 1;
                        }
                        _ => {}
                    }
                }
                b'\n' => {
                    self.flush_plain(&mut plain);
                    self.active_buffer_mut().line_feed();
                    index += 1;
                }
                b'\r' => {
                    self.flush_plain(&mut plain);
                    self.active_buffer_mut().carriage_return();
                    index += 1;
                }
                0x07 => {
                    self.flush_plain(&mut plain);
                    index += 1;
                }
                0x08 => {
                    self.flush_plain(&mut plain);
                    self.active_buffer_mut().backspace();
                    index += 1;
                }
                b'\t' => {
                    self.flush_plain(&mut plain);
                    self.active_buffer_mut().tab();
                    index += 1;
                }
                byte => {
                    plain.push(byte);
                    index += 1;
                }
            }
        }

        self.flush_plain(&mut plain);
        replies
    }

    pub fn snapshot(&self) -> ScreenSnapshot {
        self.active_buffer().snapshot(
            self.alternate_screen_active,
            self.window_title.clone(),
            self.cursor_visible,
        )
    }

    pub fn state(&self) -> ScreenState {
        ScreenState {
            normal: self
                .normal
                .snapshot(false, self.window_title.clone(), self.cursor_visible),
            alternate: self.alternate.snapshot(
                true,
                self.window_title.clone(),
                self.cursor_visible,
            ),
            alternate_screen_active: self.alternate_screen_active,
            application_cursor_keys: self.application_cursor_keys,
        }
    }

    pub fn application_cursor_keys(&self) -> bool {
        self.application_cursor_keys
    }

    fn active_buffer(&self) -> &ScreenBuffer {
        if self.alternate_screen_active {
            &self.alternate
        } else {
            &self.normal
        }
    }

    fn active_buffer_mut(&mut self) -> &mut ScreenBuffer {
        if self.alternate_screen_active {
            &mut self.alternate
        } else {
            &mut self.normal
        }
    }

    fn flush_plain(&mut self, plain: &mut Vec<u8>) {
        if plain.is_empty() {
            return;
        }

        let mut input = Vec::with_capacity(self.pending_utf8.len() + plain.len());
        input.extend_from_slice(&self.pending_utf8);
        input.extend_from_slice(plain);
        self.pending_utf8.clear();

        for ch in decode_utf8_chars(&input, &mut self.pending_utf8) {
            self.active_buffer_mut().put_char(ch);
        }
        plain.clear();
    }

    fn consume_csi(
        &mut self,
        bytes: &[u8],
        mut index: usize,
        replies: &mut Vec<u8>,
    ) -> Option<usize> {
        let start = index;
        while index < bytes.len() {
            let byte = bytes[index];
            if (0x40..=0x7e).contains(&byte) {
                let params = &bytes[start..index];
                self.handle_csi(params, byte as char, replies);
                return Some(index + 1);
            }
            index += 1;
        }

        None
    }

    fn consume_osc(
        &mut self,
        bytes: &[u8],
        mut index: usize,
        replies: &mut Vec<u8>,
    ) -> Option<usize> {
        let start = index;
        while index < bytes.len() {
            match bytes[index] {
                0x07 => {
                    self.handle_osc(&bytes[start..index], replies);
                    return Some(index + 1);
                }
                0x1b if index + 1 < bytes.len() && bytes[index + 1] == b'\\' => {
                    self.handle_osc(&bytes[start..index], replies);
                    return Some(index + 2);
                }
                _ => index += 1,
            }
        }

        None
    }

    fn handle_osc(&mut self, payload: &[u8], replies: &mut Vec<u8>) {
        let text = String::from_utf8_lossy(payload);
        let Some((kind, value)) = text.split_once(';') else {
            return;
        };
        if matches!(kind, "0" | "2") && !value.trim().is_empty() {
            self.window_title = Some(value.to_string());
        } else if value == "?" {
            match kind {
                "10" => replies.extend_from_slice(b"\x1b]10;rgb:ffff/ffff/ffff\x1b\\"),
                "11" => replies.extend_from_slice(b"\x1b]11;rgb:0000/0000/0000\x1b\\"),
                _ => {}
            }
        }
    }

    fn handle_csi(&mut self, params: &[u8], final_byte: char, replies: &mut Vec<u8>) {
        let params_text = String::from_utf8_lossy(params);

        if let Some(private_params) = params_text.strip_prefix('?') {
            self.handle_private_mode(private_params, final_byte, replies);
            return;
        }

        let numbers = parse_csi_numbers(&params_text);
        match final_byte {
            'A' => self
                .active_buffer_mut()
                .move_cursor_relative(-(first_or(&numbers, 1) as isize), 0),
            'B' => self
                .active_buffer_mut()
                .move_cursor_relative(first_or(&numbers, 1) as isize, 0),
            'C' => self
                .active_buffer_mut()
                .move_cursor_relative(0, first_or(&numbers, 1) as isize),
            'D' => self
                .active_buffer_mut()
                .move_cursor_relative(0, -(first_or(&numbers, 1) as isize)),
            'H' | 'f' => {
                let row = first_or(&numbers, 1).saturating_sub(1);
                let col = second_or(&numbers, 1).saturating_sub(1);
                self.active_buffer_mut().move_cursor_to(row, col);
            }
            'J' => self.active_buffer_mut().clear_screen(first_or(&numbers, 0)),
            'K' => self.active_buffer_mut().clear_line(first_or(&numbers, 0)),
            'S' => self.active_buffer_mut().scroll_up_in_region(first_or(&numbers, 1)),
            'r' => {
                if numbers.is_empty() {
                    self.active_buffer_mut().reset_scroll_region();
                } else {
                    let top = first_or(&numbers, 1).saturating_sub(1);
                    let bottom = second_or(&numbers, self.active_buffer().size.rows)
                        .saturating_sub(1);
                    self.active_buffer_mut().set_scroll_region(top, bottom);
                }
            }
            'c' if numbers.is_empty() || numbers == [0] => {
                replies.extend_from_slice(b"\x1b[?61;1;21;22c");
            }
            'n' if numbers == [6] => {
                let snapshot = self.active_buffer().snapshot(
                    self.alternate_screen_active,
                    self.window_title.clone(),
                    self.cursor_visible,
                );
                let row = snapshot.cursor_row.saturating_add(1);
                let col = snapshot.cursor_col.saturating_add(1);
                replies.extend_from_slice(format!("\x1b[{row};{col}R").as_bytes());
            }
            'm' => self.active_buffer_mut().apply_sgr(&numbers),
            _ => {}
        }
    }

    fn handle_private_mode(&mut self, params: &str, final_byte: char, _replies: &mut Vec<u8>) {
        if params == "1049" {
            match final_byte {
                'h' => self.alternate_screen_active = true,
                'l' => self.alternate_screen_active = false,
                _ => {}
            }
        } else if params == "1" {
            match final_byte {
                'h' => self.application_cursor_keys = true,
                'l' => self.application_cursor_keys = false,
                _ => {}
            }
        } else if params == "25" {
            match final_byte {
                'h' => self.cursor_visible = true,
                'l' => self.cursor_visible = false,
                _ => {}
            }
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ScreenCell {
    ch: char,
    style: TextStyle,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct TextStyle {
    bold: bool,
    italic: bool,
    underline: bool,
    inverse: bool,
    foreground: Option<ColorValue>,
    background: Option<ColorValue>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ColorValue {
    Indexed(u8),
    Rgb(u8, u8, u8),
}

impl TextStyle {
    fn to_ansi(self) -> String {
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

#[derive(Debug, Clone)]
struct ScreenBuffer {
    size: TerminalSize,
    cells: Vec<Vec<ScreenCell>>,
    cursor_row: u16,
    cursor_col: u16,
    scroll_top: u16,
    scroll_bottom: u16,
    scrollback: Vec<String>,
    current_style: TextStyle,
}

impl ScreenBuffer {
    fn new(size: TerminalSize) -> Self {
        Self {
            size,
            cells: blank_cells(size),
            cursor_row: 0,
            cursor_col: 0,
            scroll_top: 0,
            scroll_bottom: size.rows.saturating_sub(1),
            scrollback: Vec::new(),
            current_style: TextStyle::default(),
        }
    }

    fn resize(&mut self, size: TerminalSize) {
        let mut next = blank_cells(size);

        for row in 0..usize::min(self.cells.len(), next.len()) {
            for col in 0..usize::min(self.cells[row].len(), next[row].len()) {
                next[row][col] = self.cells[row][col];
            }
        }

        self.size = size;
        self.cells = next;
        self.cursor_row = self.cursor_row.min(size.rows.saturating_sub(1));
        self.cursor_col = self.cursor_col.min(size.cols.saturating_sub(1));
        self.scroll_top = self.scroll_top.min(size.rows.saturating_sub(1));
        self.scroll_bottom = self
            .scroll_bottom
            .max(self.scroll_top)
            .min(size.rows.saturating_sub(1));
    }

    fn snapshot(
        &self,
        alternate_screen: bool,
        window_title: Option<String>,
        cursor_visible: bool,
    ) -> ScreenSnapshot {
        ScreenSnapshot {
            size: self.size,
            lines: self
                .cells
                .iter()
                .map(|row| {
                    row.iter()
                        .filter(|cell| cell.ch != WIDE_CONTINUATION)
                        .map(|cell| cell.ch)
                        .collect::<String>()
                })
                .collect(),
            styled_lines: self
                .cells
                .iter()
                .map(|row| render_styled_row(row))
                .collect(),
            active_style_ansi: self.current_style.to_ansi(),
            scrollback: self.scrollback.clone(),
            window_title,
            cursor_row: self.cursor_row,
            cursor_col: self.cursor_col,
            cursor_visible,
            alternate_screen,
        }
    }

    fn put_char(&mut self, ch: char) {
        if self.size.rows == 0 || self.size.cols == 0 {
            return;
        }

        let width = char_display_width(ch);
        if width == 0 {
            return;
        }

        let row = self.cursor_row as usize;
        let col = self.cursor_col as usize;
        self.clear_wide_overlap(row, col);
        self.cells[row][col] = ScreenCell {
            ch,
            style: self.current_style,
        };

        if width == 2 && self.size.cols > 1 {
            if self.cursor_col + 1 >= self.size.cols {
                self.cursor_col = 0;
                self.line_feed();
                let row = self.cursor_row as usize;
                self.clear_wide_overlap(row, self.cursor_col as usize);
                self.cells[row][self.cursor_col as usize] = ScreenCell {
                    ch,
                    style: self.current_style,
                };
                self.cells[row][self.cursor_col as usize + 1] = ScreenCell {
                    ch: WIDE_CONTINUATION,
                    style: self.current_style,
                };
                self.cursor_col += 2;
            } else {
                self.cells[row][col + 1] = ScreenCell {
                    ch: WIDE_CONTINUATION,
                    style: self.current_style,
                };
                self.cursor_col += 2;
            }
        } else {
            self.cursor_col += 1;
        }

        if self.cursor_col >= self.size.cols {
            self.cursor_col = 0;
            self.line_feed();
        }
    }

    fn carriage_return(&mut self) {
        self.cursor_col = 0;
    }

    fn line_feed(&mut self) {
        if self.size.rows == 0 {
            return;
        }

        if self.cursor_row >= self.scroll_bottom {
            self.scroll_up_in_region(1);
            self.cursor_row = self.scroll_bottom;
        } else {
            self.cursor_row += 1;
        }
    }

    fn reverse_index(&mut self) {
        if self.size.rows == 0 {
            return;
        }

        if self.cursor_row <= self.scroll_top {
            self.scroll_down_in_region(1);
            self.cursor_row = self.scroll_top;
        } else {
            self.cursor_row -= 1;
        }
    }

    fn backspace(&mut self) {
        self.cursor_col = self.cursor_col.saturating_sub(1);
    }

    fn tab(&mut self) {
        if self.size.cols == 0 {
            return;
        }

        let next_tab_stop = ((self.cursor_col / 8) + 1) * 8;
        self.cursor_col = next_tab_stop.min(self.size.cols.saturating_sub(1));
    }

    fn move_cursor_to(&mut self, row: u16, col: u16) {
        self.cursor_row = row.min(self.size.rows.saturating_sub(1));
        self.cursor_col = col.min(self.size.cols.saturating_sub(1));
    }

    fn move_cursor_relative(&mut self, row_delta: isize, col_delta: isize) {
        let next_row = (self.cursor_row as isize + row_delta)
            .clamp(0, self.size.rows.saturating_sub(1) as isize) as u16;
        let next_col = (self.cursor_col as isize + col_delta)
            .clamp(0, self.size.cols.saturating_sub(1) as isize) as u16;
        self.cursor_row = next_row;
        self.cursor_col = next_col;
    }

    fn set_scroll_region(&mut self, top: u16, bottom: u16) {
        if self.size.rows == 0 {
            self.scroll_top = 0;
            self.scroll_bottom = 0;
            self.cursor_row = 0;
            self.cursor_col = 0;
            return;
        }

        let max_row = self.size.rows.saturating_sub(1);
        let top = top.min(max_row);
        let bottom = bottom.max(top).min(max_row);
        self.scroll_top = top;
        self.scroll_bottom = bottom;
        self.cursor_row = top;
        self.cursor_col = 0;
    }

    fn reset_scroll_region(&mut self) {
        self.set_scroll_region(0, self.size.rows.saturating_sub(1));
    }

    fn apply_sgr(&mut self, params: &[u16]) {
        if params.is_empty() {
            self.current_style = TextStyle::default();
            return;
        }

        let mut index = 0;
        while index < params.len() {
            match params[index] {
                0 => self.current_style = TextStyle::default(),
                1 => self.current_style.bold = true,
                3 => self.current_style.italic = true,
                4 => self.current_style.underline = true,
                7 => self.current_style.inverse = true,
                22 => self.current_style.bold = false,
                23 => self.current_style.italic = false,
                24 => self.current_style.underline = false,
                27 => self.current_style.inverse = false,
                30..=37 => {
                    self.current_style.foreground =
                        Some(ColorValue::Indexed((params[index] - 30) as u8));
                }
                39 => self.current_style.foreground = None,
                40..=47 => {
                    self.current_style.background =
                        Some(ColorValue::Indexed((params[index] - 40) as u8));
                }
                49 => self.current_style.background = None,
                90..=97 => {
                    self.current_style.foreground =
                        Some(ColorValue::Indexed((params[index] - 90 + 8) as u8));
                }
                100..=107 => {
                    self.current_style.background =
                        Some(ColorValue::Indexed((params[index] - 100 + 8) as u8));
                }
                38 | 48 => {
                    let target_foreground = params[index] == 38;
                    match params.get(index + 1).copied() {
                        Some(5) => {
                            if let Some(value) = params.get(index + 2).copied() {
                                let color = ColorValue::Indexed(value.min(255) as u8);
                                if target_foreground {
                                    self.current_style.foreground = Some(color);
                                } else {
                                    self.current_style.background = Some(color);
                                }
                                index += 2;
                            }
                        }
                        Some(2) => {
                            if let (Some(r), Some(g), Some(b)) = (
                                params.get(index + 2).copied(),
                                params.get(index + 3).copied(),
                                params.get(index + 4).copied(),
                            ) {
                                let color = ColorValue::Rgb(
                                    r.min(255) as u8,
                                    g.min(255) as u8,
                                    b.min(255) as u8,
                                );
                                if target_foreground {
                                    self.current_style.foreground = Some(color);
                                } else {
                                    self.current_style.background = Some(color);
                                }
                                index += 4;
                            }
                        }
                        _ => {}
                    }
                }
                _ => {}
            }

            index += 1;
        }
    }

    fn scroll_up_in_region(&mut self, count: u16) {
        if self.cells.is_empty() || self.size.rows == 0 {
            return;
        }

        let top = self.scroll_top as usize;
        let bottom = self.scroll_bottom as usize;
        if top >= self.cells.len() || bottom >= self.cells.len() || top > bottom {
            return;
        }

        let rows = bottom - top + 1;
        let count = usize::min(count as usize, rows);
        let full_screen_region = top == 0 && bottom + 1 == self.cells.len();

        for _ in 0..count {
            let removed = self.cells.remove(top);
            if full_screen_region {
                self.scrollback.push(
                    removed
                        .into_iter()
                        .filter(|cell| cell.ch != WIDE_CONTINUATION)
                        .map(|cell| cell.ch)
                        .collect(),
                );
            }
            self.cells
                .insert(bottom, blank_row_with_style(self.size.cols, self.current_style));
        }
    }

    fn scroll_down_in_region(&mut self, count: u16) {
        if self.cells.is_empty() || self.size.rows == 0 {
            return;
        }

        let top = self.scroll_top as usize;
        let bottom = self.scroll_bottom as usize;
        if top >= self.cells.len() || bottom >= self.cells.len() || top > bottom {
            return;
        }

        let rows = bottom - top + 1;
        let count = usize::min(count as usize, rows);

        for _ in 0..count {
            self.cells.remove(bottom);
            self.cells
                .insert(top, blank_row_with_style(self.size.cols, self.current_style));
        }
    }

    fn clear_screen(&mut self, mode: u16) {
        match mode {
            0 => {
                for row in self.cursor_row as usize..self.cells.len() {
                    let start_col = if row == self.cursor_row as usize {
                        self.cursor_col as usize
                    } else {
                        0
                    };
                    for col in start_col..self.cells[row].len() {
                        self.cells[row][col] = blank_cell_with_style(self.current_style);
                    }
                }
            }
            1 => {
                for row in 0..=self.cursor_row as usize {
                    let end_col = if row == self.cursor_row as usize {
                        self.cursor_col as usize
                    } else {
                        self.cells[row].len().saturating_sub(1)
                    };
                    for col in 0..=end_col {
                        self.cells[row][col] = blank_cell_with_style(self.current_style);
                    }
                }
            }
            _ => {
                self.cells = blank_cells_with_style(self.size, self.current_style);
                self.cursor_row = 0;
                self.cursor_col = 0;
            }
        }
    }

    fn clear_line(&mut self, mode: u16) {
        let row = self.cursor_row as usize;
        match mode {
            0 => {
                for col in self.cursor_col as usize..self.cells[row].len() {
                    self.cells[row][col] = blank_cell_with_style(self.current_style);
                }
            }
            1 => {
                for col in 0..=self.cursor_col as usize {
                    self.cells[row][col] = blank_cell_with_style(self.current_style);
                }
            }
            _ => {
                for cell in &mut self.cells[row] {
                    *cell = blank_cell_with_style(self.current_style);
                }
            }
        }
    }

    fn clear_wide_overlap(&mut self, row: usize, col: usize) {
        if self.cells[row][col].ch == WIDE_CONTINUATION {
            self.cells[row][col] = blank_cell();
            if col > 0 {
                self.cells[row][col - 1] = blank_cell();
            }
        } else if col + 1 < self.cells[row].len()
            && self.cells[row][col + 1].ch == WIDE_CONTINUATION
        {
            self.cells[row][col + 1] = blank_cell();
        }
    }
}

const WIDE_CONTINUATION: char = '\0';

fn decode_utf8_chars(bytes: &[u8], pending_utf8: &mut Vec<u8>) -> Vec<char> {
    let mut output = Vec::new();
    let mut index = 0;

    while index < bytes.len() {
        match std::str::from_utf8(&bytes[index..]) {
            Ok(valid) => {
                output.extend(valid.chars());
                break;
            }
            Err(error) => {
                let valid_up_to = error.valid_up_to();
                if valid_up_to > 0 {
                    if let Ok(valid) = std::str::from_utf8(&bytes[index..index + valid_up_to]) {
                        output.extend(valid.chars());
                    }
                    index += valid_up_to;
                    continue;
                }

                match error.error_len() {
                    Some(invalid_len) => {
                        output.push(char::REPLACEMENT_CHARACTER);
                        index += invalid_len;
                    }
                    None => {
                        pending_utf8.extend_from_slice(&bytes[index..]);
                        break;
                    }
                }
            }
        }
    }

    output
}

fn char_display_width(ch: char) -> u16 {
    if ch.is_control() {
        0
    } else if matches!(
        ch as u32,
        0x1100..=0x115F
            | 0x2329..=0x232A
            | 0x2E80..=0xA4CF
            | 0xAC00..=0xD7A3
            | 0xF900..=0xFAFF
            | 0xFE10..=0xFE19
            | 0xFE30..=0xFE6F
            | 0xFF00..=0xFF60
            | 0xFFE0..=0xFFE6
            | 0x1F300..=0x1FAFF
    ) {
        2
    } else {
        1
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

fn blank_cells(size: TerminalSize) -> Vec<Vec<ScreenCell>> {
    blank_cells_with_style(size, TextStyle::default())
}

fn blank_cells_with_style(size: TerminalSize, style: TextStyle) -> Vec<Vec<ScreenCell>> {
    (0..size.rows)
        .map(|_| blank_row_with_style(size.cols, style))
        .collect()
}

fn blank_row(cols: u16) -> Vec<ScreenCell> {
    blank_row_with_style(cols, TextStyle::default())
}

fn blank_row_with_style(cols: u16, style: TextStyle) -> Vec<ScreenCell> {
    vec![blank_cell_with_style(style); cols as usize]
}

fn blank_cell() -> ScreenCell {
    blank_cell_with_style(TextStyle::default())
}

fn blank_cell_with_style(style: TextStyle) -> ScreenCell {
    ScreenCell { ch: ' ', style }
}

fn render_styled_row(row: &[ScreenCell]) -> String {
    let mut rendered = String::new();
    let mut active_style = TextStyle::default();

    for cell in row.iter().filter(|cell| cell.ch != WIDE_CONTINUATION) {
        if cell.style != active_style {
            rendered.push_str(&cell.style.to_ansi());
            active_style = cell.style;
        }
        rendered.push(cell.ch);
    }

    if active_style != TextStyle::default() {
        rendered.push_str("\x1b[0m");
    }

    rendered
}

fn parse_csi_numbers(params: &str) -> Vec<u16> {
    if params.is_empty() {
        return Vec::new();
    }

    params
        .split(';')
        .map(|value| value.parse::<u16>().unwrap_or(0))
        .collect()
}

fn first_or(values: &[u16], default: u16) -> u16 {
    values.first().copied().unwrap_or(default)
}

fn second_or(values: &[u16], default: u16) -> u16 {
    values.get(1).copied().unwrap_or(default)
}

fn is_tty(fd: RawFd) -> bool {
    unsafe { isatty(fd as c_int) == 1 }
}

fn read_termios(fd: RawFd) -> Result<Termios, TerminalError> {
    let mut termios = MaybeUninit::<Termios>::uninit();
    let result = unsafe { tcgetattr(fd as c_int, termios.as_mut_ptr()) };
    if result != 0 {
        return Err(TerminalError::Io(
            "failed to read terminal mode".to_string(),
            io::Error::last_os_error(),
        ));
    }

    Ok(unsafe { termios.assume_init() })
}

fn write_termios(fd: RawFd, termios: &Termios) -> Result<(), TerminalError> {
    let result = unsafe { tcsetattr(fd as c_int, TCSAFLUSH, termios as *const Termios) };
    if result != 0 {
        return Err(TerminalError::Io(
            "failed to write terminal mode".to_string(),
            io::Error::last_os_error(),
        ));
    }

    Ok(())
}

fn write_escape(fd: RawFd, value: &str) -> Result<(), TerminalError> {
    let path = format!("/proc/self/fd/{fd}");
    let mut handle = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(|error| {
            TerminalError::Io("failed to open terminal output stream".to_string(), error)
        })?;
    handle
        .write_all(value.as_bytes())
        .map_err(|error| TerminalError::Io("failed to write terminal escape".to_string(), error))?;
    handle
        .flush()
        .map_err(|error| TerminalError::Io("failed to flush terminal escape".to_string(), error))
}

fn make_raw(mut termios: Termios) -> Termios {
    termios.c_iflag &= !(BRKINT | ICRNL | INPCK | ISTRIP | IXON);
    termios.c_oflag &= !OPOST;
    termios.c_cflag |= CS8;
    termios.c_lflag &= !(ECHO | ICANON | IEXTEN | ISIG);
    termios.c_cc[VMIN] = 1;
    termios.c_cc[VTIME] = 0;
    termios
}

#[cfg(test)]
mod tests {
    use super::{
        make_raw, ResizeTracker, TerminalEngine, TerminalSize, Termios, BRKINT, ECHO, ICANON,
        ICRNL, IEXTEN, INPCK, ISIG, ISTRIP, IXON, OPOST, VMIN, VTIME,
    };

    #[test]
    fn resize_tracker_reports_only_real_changes() {
        let mut tracker = ResizeTracker::default();
        let initial = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        };
        let updated = TerminalSize {
            rows: 40,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        };

        assert_eq!(tracker.observe(initial), Some(initial));
        assert_eq!(tracker.observe(initial), None);
        assert_eq!(tracker.observe(updated), Some(updated));
    }

    #[test]
    fn make_raw_disables_canonical_input_flags() {
        let mut termios = Termios {
            c_iflag: BRKINT | ICRNL | INPCK | ISTRIP | IXON,
            c_oflag: OPOST,
            c_cflag: 0,
            c_lflag: ECHO | ICANON | IEXTEN | ISIG,
            c_line: 0,
            c_cc: [0; 32],
            c_ispeed: 0,
            c_ospeed: 0,
        };
        termios.c_cc[VMIN] = 0;
        termios.c_cc[VTIME] = 10;

        let raw = make_raw(termios);

        assert_eq!(raw.c_iflag & (BRKINT | ICRNL | INPCK | ISTRIP | IXON), 0);
        assert_eq!(raw.c_oflag & OPOST, 0);
        assert_eq!(raw.c_lflag & (ECHO | ICANON | IEXTEN | ISIG), 0);
        assert_eq!(raw.c_cc[VMIN], 1);
        assert_eq!(raw.c_cc[VTIME], 0);
    }

    #[test]
    fn engine_tracks_plain_text_and_cursor_state() {
        let mut engine = TerminalEngine::new(TerminalSize {
            rows: 2,
            cols: 6,
            pixel_width: 0,
            pixel_height: 0,
        });

        engine.feed(b"hello");
        let snapshot = engine.snapshot();

        assert_eq!(snapshot.lines[0], "hello ");
        assert_eq!(snapshot.cursor_row, 0);
        assert_eq!(snapshot.cursor_col, 5);
        assert!(snapshot.cursor_visible);
        assert!(!snapshot.alternate_screen);
    }

    #[test]
    fn engine_snapshot_preserves_ansi_sgr_styling() {
        let mut engine = TerminalEngine::new(TerminalSize {
            rows: 2,
            cols: 16,
            pixel_width: 0,
            pixel_height: 0,
        });

        engine.feed(b"\x1b[38;5;196mred\x1b[0m plain");
        let snapshot = engine.snapshot();

        assert_eq!(snapshot.lines[0], "red plain       ");
        assert!(
            snapshot.styled_lines[0].starts_with("\x1b[0;38;5;196mred\x1b[0m plain"),
            "styled line should preserve the foreground color: {:?}",
            snapshot.styled_lines[0]
        );
        assert_eq!(snapshot.active_style_ansi, "\x1b[0m");
    }

    #[test]
    fn engine_snapshot_preserves_active_sgr_for_future_output() {
        let mut engine = TerminalEngine::new(TerminalSize {
            rows: 2,
            cols: 16,
            pixel_width: 0,
            pixel_height: 0,
        });

        engine.feed(b"\x1b[38;5;196mred");
        let snapshot = engine.snapshot();

        assert_eq!(snapshot.lines[0], "red             ");
        assert!(
            snapshot.styled_lines[0].starts_with("\x1b[0;38;5;196mred"),
            "styled line should preserve the foreground color: {:?}",
            snapshot.styled_lines[0]
        );
        assert_eq!(snapshot.active_style_ansi, "\x1b[0;38;5;196m");
    }

    #[test]
    fn engine_preserves_split_utf8_sequences() {
        let mut engine = TerminalEngine::new(TerminalSize {
            rows: 2,
            cols: 6,
            pixel_width: 0,
            pixel_height: 0,
        });

        engine.feed(&[0xE4, 0xBD]);
        engine.feed(&[0xA0, b'a']);
        let snapshot = engine.snapshot();

        assert_eq!(snapshot.lines[0], "你a   ");
        assert_eq!(snapshot.cursor_col, 3);
    }

    #[test]
    fn engine_tracks_wide_character_cursor_width() {
        let mut engine = TerminalEngine::new(TerminalSize {
            rows: 2,
            cols: 6,
            pixel_width: 0,
            pixel_height: 0,
        });

        engine.feed("你好".as_bytes());
        let snapshot = engine.snapshot();

        assert_eq!(snapshot.lines[0], "你好  ");
        assert_eq!(snapshot.cursor_col, 4);
    }

    #[test]
    fn engine_handles_carriage_return_and_cursor_positioning() {
        let mut engine = TerminalEngine::new(TerminalSize {
            rows: 2,
            cols: 6,
            pixel_width: 0,
            pixel_height: 0,
        });

        engine.feed(b"hello\rHE");
        engine.feed(b"\x1b[2;3H!");
        let snapshot = engine.snapshot();

        assert_eq!(snapshot.lines[0], "HEllo ");
        assert_eq!(snapshot.lines[1], "  !   ");
    }

    #[test]
    fn engine_handles_clear_line_and_clear_screen() {
        let mut engine = TerminalEngine::new(TerminalSize {
            rows: 2,
            cols: 6,
            pixel_width: 0,
            pixel_height: 0,
        });

        engine.feed(b"hello\x1b[2K");
        let snapshot = engine.snapshot();
        assert_eq!(snapshot.lines[0], "      ");

        engine.feed(b"\x1b[2J");
        let cleared = engine.snapshot();
        assert_eq!(
            cleared.lines,
            vec!["      ".to_string(), "      ".to_string()]
        );
        assert_eq!(cleared.cursor_row, 0);
        assert_eq!(cleared.cursor_col, 0);
    }

    #[test]
    fn engine_handles_split_csi_sequences_across_feed_calls() {
        let mut engine = TerminalEngine::new(TerminalSize {
            rows: 2,
            cols: 16,
            pixel_width: 0,
            pixel_height: 0,
        });

        engine.feed(b"echo abc");
        engine.feed(b"\x08\x1b[");
        engine.feed(b"K");
        let snapshot = engine.snapshot();

        assert_eq!(snapshot.lines[0], "echo ab         ");
        assert_eq!(snapshot.cursor_col, 7);
    }

    #[test]
    fn engine_handles_scroll_region_and_scroll_up() {
        let mut engine = TerminalEngine::new(TerminalSize {
            rows: 5,
            cols: 8,
            pixel_width: 0,
            pixel_height: 0,
        });

        engine.feed(
            b"row1\r\nrow2\r\nrow3\r\nrow4\r\nrow5\x1b[1;3r\x1b[2S\x1b[r",
        );
        let snapshot = engine.snapshot();

        assert_eq!(snapshot.lines[0], "row3    ");
        assert_eq!(snapshot.lines[1], "        ");
        assert_eq!(snapshot.lines[2], "        ");
        assert_eq!(snapshot.lines[3], "row4    ");
        assert_eq!(snapshot.lines[4], "row5    ");
    }

    #[test]
    fn engine_line_feed_respects_scroll_region() {
        let mut engine = TerminalEngine::new(TerminalSize {
            rows: 4,
            cols: 8,
            pixel_width: 0,
            pixel_height: 0,
        });

        engine.feed(b"top\r\nmid\r\nbot");
        engine.feed(b"\x1b[2;3r\x1b[3;1H!\n");
        let snapshot = engine.snapshot();

        assert_eq!(snapshot.lines[0], "top     ");
        assert_eq!(snapshot.lines[1], "!ot     ");
        assert_eq!(snapshot.lines[2], "        ");
    }

    #[test]
    fn engine_reverse_index_respects_scroll_region() {
        let mut engine = TerminalEngine::new(TerminalSize {
            rows: 4,
            cols: 8,
            pixel_width: 0,
            pixel_height: 0,
        });

        engine.feed(b"row1\r\nrow2\r\nrow3\r\nrow4");
        engine.feed(b"\x1b[2;4r\x1b[2;1H\x1bM");
        let snapshot = engine.snapshot();

        assert_eq!(snapshot.lines[0], "row1    ");
        assert_eq!(snapshot.lines[1], "        ");
        assert_eq!(snapshot.lines[2], "row2    ");
        assert_eq!(snapshot.lines[3], "row3    ");
        assert_eq!(snapshot.cursor_row, 1);
        assert_eq!(snapshot.cursor_col, 0);
    }

    #[test]
    fn engine_ignores_bell_without_advancing_cursor() {
        let mut engine = TerminalEngine::new(TerminalSize {
            rows: 2,
            cols: 8,
            pixel_width: 0,
            pixel_height: 0,
        });

        engine.feed(b"abc\x07\x07");
        let snapshot = engine.snapshot();

        assert_eq!(snapshot.lines[0], "abc     ");
        assert_eq!(snapshot.cursor_col, 3);
    }

    #[test]
    fn engine_replies_to_terminal_capability_queries() {
        let mut engine = TerminalEngine::new(TerminalSize {
            rows: 2,
            cols: 20,
            pixel_width: 0,
            pixel_height: 0,
        });

        let replies = engine.feed_and_collect_replies(b"\x1b[6n\x1b[c\x1b[?u\x1b]10;?\x1b\\");

        let reply_text = String::from_utf8_lossy(&replies);
        assert!(reply_text.contains("\x1b[1;1R"));
        assert!(reply_text.contains("\x1b[?61;1;21;22c"));
        assert!(!reply_text.contains("\x1b[?0u"));
        assert!(reply_text.contains("\x1b]10;rgb:ffff/ffff/ffff\x1b\\"));
    }

    #[test]
    fn engine_tracks_application_cursor_mode() {
        let mut engine = TerminalEngine::new(TerminalSize {
            rows: 2,
            cols: 20,
            pixel_width: 0,
            pixel_height: 0,
        });

        engine.feed(b"\x1b[?1h");
        assert!(engine.application_cursor_keys());

        engine.feed(b"\x1b[?1l");
        assert!(!engine.application_cursor_keys());
    }

    #[test]
    fn engine_tracks_cursor_visibility() {
        let mut engine = TerminalEngine::new(TerminalSize {
            rows: 2,
            cols: 20,
            pixel_width: 0,
            pixel_height: 0,
        });

        engine.feed(b"\x1b[?25l");
        assert!(!engine.snapshot().cursor_visible);

        engine.feed(b"\x1b[?25h");
        assert!(engine.snapshot().cursor_visible);
    }

    #[test]
    fn engine_tracks_scrollback_when_screen_overflows() {
        let mut engine = TerminalEngine::new(TerminalSize {
            rows: 2,
            cols: 5,
            pixel_width: 0,
            pixel_height: 0,
        });

        engine.feed(b"one\r\ntwo\r\nthree");
        let snapshot = engine.snapshot();

        assert_eq!(
            snapshot.scrollback,
            vec!["one  ".to_string(), "two  ".to_string()]
        );
        assert_eq!(snapshot.lines[0], "three");
    }

    #[test]
    fn engine_preserves_normal_and_alternate_screens() {
        let mut engine = TerminalEngine::new(TerminalSize {
            rows: 2,
            cols: 6,
            pixel_width: 0,
            pixel_height: 0,
        });

        engine.feed(b"main");
        engine.feed(b"\x1b[?1049h");
        engine.feed(b"alt");
        let alternate = engine.snapshot();
        assert!(alternate.alternate_screen);
        assert_eq!(alternate.lines[0], "alt   ");

        engine.feed(b"\x1b[?1049l");
        let normal = engine.snapshot();
        assert!(!normal.alternate_screen);
        assert_eq!(normal.lines[0], "main  ");
    }

    #[test]
    fn engine_ignores_osc_window_title_sequences() {
        let mut engine = TerminalEngine::new(TerminalSize {
            rows: 2,
            cols: 20,
            pixel_width: 0,
            pixel_height: 0,
        });

        engine.feed(b"\x1b]0;k@k: /tmp\x07prompt$ ");
        let snapshot = engine.snapshot();

        assert_eq!(snapshot.lines[0], "prompt$             ");
        assert_eq!(snapshot.window_title.as_deref(), Some("k@k: /tmp"));
    }

    #[test]
    fn engine_ignores_osc_sequences_terminated_by_st() {
        let mut engine = TerminalEngine::new(TerminalSize {
            rows: 2,
            cols: 20,
            pixel_width: 0,
            pixel_height: 0,
        });

        engine.feed(b"\x1b]0;session title\x1b\\ready");
        let snapshot = engine.snapshot();

        assert_eq!(snapshot.lines[0], "ready               ");
        assert_eq!(snapshot.window_title.as_deref(), Some("session title"));
    }

    #[test]
    fn engine_handles_split_osc_window_title_sequences() {
        let mut engine = TerminalEngine::new(TerminalSize {
            rows: 2,
            cols: 20,
            pixel_width: 0,
            pixel_height: 0,
        });

        engine.feed(b"\x1b]0;k@k: /tm");
        engine.feed(b"p\x07prompt$ ");
        let snapshot = engine.snapshot();

        assert_eq!(snapshot.lines[0], "prompt$             ");
        assert_eq!(snapshot.window_title.as_deref(), Some("k@k: /tmp"));
    }

    #[test]
    fn engine_resize_preserves_visible_prefix() {
        let mut engine = TerminalEngine::new(TerminalSize {
            rows: 2,
            cols: 6,
            pixel_width: 0,
            pixel_height: 0,
        });

        engine.feed(b"hello\r\nworld");
        engine.resize(TerminalSize {
            rows: 3,
            cols: 4,
            pixel_width: 0,
            pixel_height: 0,
        });
        let snapshot = engine.snapshot();

        assert_eq!(snapshot.lines[0], "hell");
        assert_eq!(snapshot.lines[1], "worl");
        assert_eq!(snapshot.size.cols, 4);
        assert_eq!(snapshot.size.rows, 3);
    }
}
