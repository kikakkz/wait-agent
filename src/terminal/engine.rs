use std::collections::VecDeque;

use super::ansi::{
    blank_cell, blank_cell_with_style, blank_cells, blank_cells_with_style, blank_row_with_style,
    char_display_width, decode_utf8_chars, first_or, parse_csi_numbers, parse_sgr_segments,
    render_plain_row, render_styled_row, second_or, translate_dec_graphics, SgrSegment,
};
use super::types::*;

/// Scrollback history is capped so long-running sessions do not grow memory
/// without bound; the plain and styled vecs are trimmed together.
const MAX_SCROLLBACK_LINES: usize = 10_000;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum CharacterSet {
    #[default]
    Ascii,
    DecGraphics,
}

#[derive(Debug, Clone, Copy, Default)]
struct CharsetState {
    g0: CharacterSet,
    g1: CharacterSet,
    use_g1: bool,
}

#[derive(Debug, Clone, Copy)]
struct WriteModes {
    autowrap: bool,
    insert: bool,
}

#[derive(Debug, Clone)]
pub struct TerminalEngine {
    normal: ScreenBuffer,
    alternate: ScreenBuffer,
    alternate_screen_active: bool,
    application_cursor_keys: bool,
    cursor_visible: bool,
    autowrap: bool,
    origin_mode: bool,
    insert_mode: bool,
    bracketed_paste: bool,
    mouse_reporting: MouseReportingMode,
    mouse_encoding: MouseEncoding,
    charset: CharsetState,
    saved_charset: Option<CharsetState>,
    window_title: Option<String>,
    osc52_queue: Vec<String>,
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
            autowrap: true,
            origin_mode: false,
            insert_mode: false,
            bracketed_paste: false,
            mouse_reporting: MouseReportingMode::None,
            mouse_encoding: MouseEncoding::X10,
            charset: CharsetState::default(),
            saved_charset: None,
            window_title: None,
            osc52_queue: Vec::new(),
            pending_escape: Vec::new(),
            pending_utf8: Vec::new(),
        }
    }

    pub fn resize(&mut self, size: TerminalSize) {
        self.normal.resize(size);
        self.alternate.resize(size);
    }

    #[cfg(test)]
    pub fn size(&self) -> TerminalSize {
        self.normal.size
    }

    #[cfg(test)]
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
                        b'(' | b')' => {
                            if index + 1 >= input.len() {
                                self.pending_escape
                                    .extend_from_slice(&input[escape_start..]);
                                break;
                            }
                            let designate_g0 = input[index] == b'(';
                            self.designate_charset(designate_g0, input[index + 1]);
                            index += 2;
                        }
                        b'7' => {
                            self.save_cursor();
                            index += 1;
                        }
                        b'8' => {
                            self.restore_cursor();
                            index += 1;
                        }
                        b'c' => {
                            self.reset();
                            index += 1;
                        }
                        b'M' => {
                            self.active_buffer_mut().reverse_index();
                            index += 1;
                        }
                        _ => {
                            index += 1;
                        }
                    }
                }
                0x0e => {
                    self.flush_plain(&mut plain);
                    self.charset.use_g1 = true;
                    index += 1;
                }
                0x0f => {
                    self.flush_plain(&mut plain);
                    self.charset.use_g1 = false;
                    index += 1;
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

    #[cfg(test)]
    pub fn snapshot(&self) -> ScreenSnapshot {
        self.active_buffer().snapshot(
            self.alternate_screen_active,
            self.window_title.clone(),
            self.cursor_visible,
        )
    }

    /// Snapshot of the visible screen for the per-frame render path; the
    /// scrollback vectors are left empty to avoid cloning history each frame.
    #[cfg(test)]
    pub fn snapshot_visible(&self) -> ScreenSnapshot {
        self.active_buffer().snapshot_visible(
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

    #[cfg(test)]
    pub fn bracketed_paste(&self) -> bool {
        self.bracketed_paste
    }

    #[cfg(test)]
    pub fn mouse_reporting(&self) -> MouseReportingMode {
        self.mouse_reporting
    }

    #[cfg(test)]
    pub fn mouse_encoding(&self) -> MouseEncoding {
        self.mouse_encoding
    }

    /// OSC 52 clipboard payloads observed since the last drain, each the full
    /// `52;...` sequence body ready to be re-emitted to the local terminal.
    #[cfg(test)]
    pub fn drain_osc52(&mut self) -> Vec<String> {
        std::mem::take(&mut self.osc52_queue)
    }

    /// Plain-text scrollback lines that have rolled off the normal screen since
    /// the last drain. Engine-mode rendering emits these to the local pane
    /// before drawing the current frame so the local terminal captures full history.
    /// Only the normal buffer is bridged: alternate-screen applications (vim,
    /// full-screen TUIs) should not pollute the local scrollback.
    #[cfg(test)]
    pub fn drain_scrollback_lines(&mut self) -> Vec<String> {
        self.normal.drain_scrollback_lines()
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

        let modes = self.write_modes();
        let dec_graphics = self.active_charset() == CharacterSet::DecGraphics;
        for ch in decode_utf8_chars(&input, &mut self.pending_utf8) {
            let ch = if dec_graphics {
                translate_dec_graphics(ch)
            } else {
                ch
            };
            self.active_buffer_mut().put_char(ch, modes);
        }
        plain.clear();
    }

    fn write_modes(&self) -> WriteModes {
        WriteModes {
            autowrap: self.autowrap,
            insert: self.insert_mode,
        }
    }

    fn active_charset(&self) -> CharacterSet {
        if self.charset.use_g1 {
            self.charset.g1
        } else {
            self.charset.g0
        }
    }

    fn designate_charset(&mut self, g0: bool, code: u8) {
        // Unknown designations (A, U, 4, ...) are treated as ASCII.
        let charset = match code {
            b'0' => CharacterSet::DecGraphics,
            _ => CharacterSet::Ascii,
        };
        if g0 {
            self.charset.g0 = charset;
        } else {
            self.charset.g1 = charset;
        }
    }

    fn save_cursor(&mut self) {
        self.active_buffer_mut().save_cursor();
        self.saved_charset = Some(self.charset);
    }

    fn restore_cursor(&mut self) {
        self.active_buffer_mut().restore_cursor();
        if let Some(saved) = self.saved_charset {
            self.charset = saved;
        }
    }

    /// RIS (`ESC c`): back to power-on state. Scrollback history is kept.
    fn reset(&mut self) {
        self.normal.reset();
        self.alternate.reset();
        self.alternate_screen_active = false;
        self.application_cursor_keys = false;
        self.cursor_visible = true;
        self.autowrap = true;
        self.origin_mode = false;
        self.insert_mode = false;
        self.bracketed_paste = false;
        self.mouse_reporting = MouseReportingMode::None;
        self.mouse_encoding = MouseEncoding::X10;
        self.charset = CharsetState::default();
        self.saved_charset = None;
    }

    fn set_alternate_screen(&mut self, active: bool, mode: u16) {
        if active == self.alternate_screen_active {
            return;
        }

        if active {
            if matches!(mode, 1048 | 1049) {
                self.save_cursor();
            }
            self.alternate_screen_active = true;
        } else {
            self.alternate_screen_active = false;
            if matches!(mode, 1048 | 1049) {
                self.restore_cursor();
            }
            if mode == 1047 {
                self.alternate.clear_screen(2);
            }
        }
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
        if kind == "52" {
            // Clipboard payload is queued verbatim for the viewer to re-emit
            // to the local terminal (base64 content untouched).
            self.osc52_queue.push(text.into_owned());
        } else if matches!(kind, "0" | "2") && !value.trim().is_empty() {
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
            'E' => {
                let buffer = self.active_buffer_mut();
                buffer.move_cursor_relative(first_or(&numbers, 1) as isize, 0);
                buffer.carriage_return();
            }
            'F' => {
                let buffer = self.active_buffer_mut();
                buffer.move_cursor_relative(-(first_or(&numbers, 1) as isize), 0);
                buffer.carriage_return();
            }
            'G' | '`' => self
                .active_buffer_mut()
                .move_cursor_col(first_or(&numbers, 1).saturating_sub(1)),
            'd' => {
                let origin = self.origin_mode;
                self.active_buffer_mut()
                    .move_cursor_row(first_or(&numbers, 1).saturating_sub(1), origin);
            }
            'H' | 'f' => {
                let row = first_or(&numbers, 1).saturating_sub(1);
                let col = second_or(&numbers, 1).saturating_sub(1);
                let origin = self.origin_mode;
                self.active_buffer_mut().move_cursor_to(row, col, origin);
            }
            'J' => self.active_buffer_mut().clear_screen(first_or(&numbers, 0)),
            'K' => self.active_buffer_mut().clear_line(first_or(&numbers, 0)),
            'L' => self.active_buffer_mut().insert_lines(first_or(&numbers, 1)),
            'M' => self.active_buffer_mut().delete_lines(first_or(&numbers, 1)),
            'P' => self.active_buffer_mut().delete_chars(first_or(&numbers, 1)),
            '@' => self.active_buffer_mut().insert_chars(first_or(&numbers, 1)),
            'X' => self.active_buffer_mut().erase_chars(first_or(&numbers, 1)),
            'b' => {
                let modes = self.write_modes();
                self.active_buffer_mut()
                    .repeat_last_char(first_or(&numbers, 1), modes);
            }
            'S' => self
                .active_buffer_mut()
                .scroll_up_in_region(first_or(&numbers, 1)),
            // `CSI > ... T` (title mode queries) is not a scroll request.
            'T' if !params_text.starts_with('>') => self
                .active_buffer_mut()
                .scroll_down_in_region(first_or(&numbers, 1)),
            'r' => {
                let origin = self.origin_mode;
                if numbers.is_empty() {
                    self.active_buffer_mut().reset_scroll_region(origin);
                } else {
                    let top = first_or(&numbers, 1).saturating_sub(1);
                    let bottom =
                        second_or(&numbers, self.active_buffer().size.rows).saturating_sub(1);
                    self.active_buffer_mut()
                        .set_scroll_region(top, bottom, origin);
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
            'm' => self
                .active_buffer_mut()
                .apply_sgr(&parse_sgr_segments(&params_text)),
            's' if numbers.is_empty() => self.save_cursor(),
            'u' if numbers.is_empty() => self.restore_cursor(),
            'h' | 'l' => self.handle_public_mode(&numbers, final_byte),
            _ => {}
        }
    }

    fn handle_public_mode(&mut self, numbers: &[u16], final_byte: char) {
        let enabled = final_byte == 'h';
        for mode in numbers {
            // Mode 4 is IRM (insert/replace); other public modes are ignored.
            if *mode == 4 {
                self.insert_mode = enabled;
            }
        }
    }

    fn handle_private_mode(&mut self, params: &str, final_byte: char, _replies: &mut [u8]) {
        let enabled = match final_byte {
            'h' => true,
            'l' => false,
            _ => return,
        };
        for mode in parse_csi_numbers(params) {
            match mode {
                1 => self.application_cursor_keys = enabled,
                6 => self.origin_mode = enabled,
                7 => self.autowrap = enabled,
                25 => self.cursor_visible = enabled,
                47 | 1047 | 1048 | 1049 => self.set_alternate_screen(enabled, mode),
                1000 => {
                    self.mouse_reporting = if enabled {
                        MouseReportingMode::Click
                    } else {
                        MouseReportingMode::None
                    };
                }
                1002 => {
                    self.mouse_reporting = if enabled {
                        MouseReportingMode::Drag
                    } else {
                        MouseReportingMode::None
                    };
                }
                1003 => {
                    self.mouse_reporting = if enabled {
                        MouseReportingMode::AnyMotion
                    } else {
                        MouseReportingMode::None
                    };
                }
                1006 => {
                    self.mouse_encoding = if enabled {
                        MouseEncoding::Sgr
                    } else {
                        MouseEncoding::X10
                    };
                }
                1015 => {
                    self.mouse_encoding = if enabled {
                        MouseEncoding::Utf8
                    } else {
                        MouseEncoding::X10
                    };
                }
                2004 => self.bracketed_paste = enabled,
                _ => {}
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
    pending_wrap: bool,
    scroll_top: u16,
    scroll_bottom: u16,
    styled_scrollback: VecDeque<String>,
    current_style: TextStyle,
    saved_cursor: SavedCursorState,
    last_char: Option<char>,
    /// Number of scrollback lines already emitted to the local pane. Used by
    /// engine-mode rendering to bridge the engine's internal scrollback to the
    /// local terminal scrollback without cloning the whole history each frame.
    scrollback_emitted_count: usize,
}

impl ScreenBuffer {
    fn new(size: TerminalSize) -> Self {
        Self {
            size,
            cells: blank_cells(size),
            cursor_row: 0,
            cursor_col: 0,
            pending_wrap: false,
            scroll_top: 0,
            scroll_bottom: size.rows.saturating_sub(1),
            styled_scrollback: VecDeque::new(),
            current_style: TextStyle::default(),
            saved_cursor: SavedCursorState::default(),
            last_char: None,
            scrollback_emitted_count: 0,
        }
    }

    /// Scrollback lines that have not been emitted yet and advance the emitted
    /// cursor. The returned lines preserve ANSI SGR escape sequences so the
    /// local terminal copy-mode can show color, bold, and other styles.
    #[cfg(test)]
    fn drain_scrollback_lines(&mut self) -> Vec<String> {
        let total = self.styled_scrollback.len();
        if total <= self.scrollback_emitted_count {
            return Vec::new();
        }
        let new_lines: Vec<String> = self
            .styled_scrollback
            .range(self.scrollback_emitted_count..total)
            .cloned()
            .collect();
        self.scrollback_emitted_count = total;
        new_lines
    }

    /// Truncates or pads the cell grid in place without reflowing content.
    /// Callers rely on the remote peer repainting the screen after SIGWINCH.
    fn resize(&mut self, size: TerminalSize) {
        if self.size == size {
            return;
        }
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
        self.pending_wrap = false;
        // Resize resets the DECSTBM scrolling region to the full new screen.
        // The remote peer is expected to repaint after SIGWINCH and can set
        // explicit margins again if it needs them; keeping old margins caused
        // the screen to use only the top half when growing from 24 to 50 rows.
        self.scroll_top = 0;
        self.scroll_bottom = size.rows.saturating_sub(1);
        if self.saved_cursor.valid {
            self.saved_cursor.row = self.saved_cursor.row.min(size.rows.saturating_sub(1));
            self.saved_cursor.col = self.saved_cursor.col.min(size.cols.saturating_sub(1));
        }
    }

    fn snapshot(
        &self,
        alternate_screen: bool,
        window_title: Option<String>,
        cursor_visible: bool,
    ) -> ScreenSnapshot {
        self.build_snapshot(alternate_screen, window_title, cursor_visible, true)
    }

    #[cfg(test)]
    fn snapshot_visible(
        &self,
        alternate_screen: bool,
        window_title: Option<String>,
        cursor_visible: bool,
    ) -> ScreenSnapshot {
        self.build_snapshot(alternate_screen, window_title, cursor_visible, false)
    }

    fn build_snapshot(
        &self,
        alternate_screen: bool,
        window_title: Option<String>,
        cursor_visible: bool,
        include_scrollback: bool,
    ) -> ScreenSnapshot {
        ScreenSnapshot {
            size: self.size,
            lines: self.cells.iter().map(|row| render_plain_row(row)).collect(),
            styled_lines: self
                .cells
                .iter()
                .map(|row| render_styled_row(row))
                .collect(),
            active_style_ansi: self.current_style.to_ansi(),
            styled_scrollback: if include_scrollback {
                self.styled_scrollback.iter().cloned().collect()
            } else {
                Vec::new()
            },
            scroll_top: self.scroll_top,
            scroll_bottom: self.scroll_bottom,
            window_title,
            cursor_row: self.cursor_row,
            cursor_col: self.cursor_col,
            cursor_visible,
            alternate_screen,
        }
    }

    /// RIS (`ESC c`): restore power-on state while keeping scrollback history.
    fn reset(&mut self) {
        self.cells = blank_cells(self.size);
        self.cursor_row = 0;
        self.cursor_col = 0;
        self.pending_wrap = false;
        self.scroll_top = 0;
        self.scroll_bottom = self.size.rows.saturating_sub(1);
        self.current_style = TextStyle::default();
        self.saved_cursor = SavedCursorState::default();
        self.last_char = None;
    }

    fn put_char(&mut self, ch: char, modes: WriteModes) {
        if self.size.rows == 0 || self.size.cols == 0 {
            return;
        }

        let width = char_display_width(ch);
        if width == 0 {
            return;
        }

        if self.pending_wrap {
            self.cursor_col = 0;
            self.line_feed();
            self.pending_wrap = false;
        }

        if modes.insert {
            self.insert_blanks_at_cursor(width);
        }

        let row = self.cursor_row as usize;
        let col = self.cursor_col as usize;
        self.clear_wide_overlap(row, col);
        self.cells[row][col] = ScreenCell {
            ch,
            style: self.current_style,
        };
        self.last_char = Some(ch);

        if width == 2 && self.size.cols > 1 {
            if self.cursor_col + 1 >= self.size.cols {
                if !modes.autowrap {
                    // Without autowrap a wide char at the right margin is
                    // clipped to the last column; the cursor stays put and no
                    // wrap is pending.
                    return;
                }
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
            } else {
                self.cells[row][col + 1] = ScreenCell {
                    ch: WIDE_CONTINUATION,
                    style: self.current_style,
                };
            }
        }

        let next = self.cursor_col + width;
        if next >= self.size.cols {
            self.cursor_col = self.size.cols.saturating_sub(1);
            self.pending_wrap = modes.autowrap;
        } else {
            self.cursor_col = next;
        }
    }

    fn repeat_last_char(&mut self, count: u16, modes: WriteModes) {
        let Some(ch) = self.last_char else {
            return;
        };
        for _ in 0..count {
            self.put_char(ch, modes);
        }
    }

    /// Shifts the cursor row's cells right by `count` at the cursor column,
    /// dropping the overflow at the end of the line (IRM / ICH semantics).
    fn insert_blanks_at_cursor(&mut self, count: u16) {
        if self.cells.is_empty() || self.size.cols == 0 {
            return;
        }

        let row = self.cursor_row as usize;
        if row >= self.cells.len() {
            return;
        }

        let start = self.cursor_col as usize;
        if start >= self.cells[row].len() {
            return;
        }

        let count = usize::min(count as usize, self.cells[row].len() - start);
        let len = self.cells[row].len();
        let row_cells = &mut self.cells[row];
        row_cells.copy_within(start..len - count, start + count);
        for cell in &mut row_cells[start..start + count] {
            *cell = blank_cell_with_style(self.current_style);
        }
    }

    fn insert_chars(&mut self, count: u16) {
        self.insert_blanks_at_cursor(count.max(1));
        self.pending_wrap = false;
    }

    fn erase_chars(&mut self, count: u16) {
        if self.cells.is_empty() || self.size.rows == 0 || self.size.cols == 0 {
            return;
        }

        let row = self.cursor_row as usize;
        if row >= self.cells.len() {
            return;
        }

        let start = self.cursor_col as usize;
        if start >= self.cells[row].len() {
            return;
        }

        let end = (start + usize::max(1, count as usize)).min(self.cells[row].len());
        for cell in &mut self.cells[row][start..end] {
            *cell = blank_cell_with_style(self.current_style);
        }
        self.pending_wrap = false;
    }

    fn insert_lines(&mut self, count: u16) {
        if self.cells.is_empty() || self.size.rows == 0 {
            return;
        }

        let row = self.cursor_row as usize;
        let top = self.scroll_top as usize;
        let bottom = self.scroll_bottom as usize;
        if row < top || row > bottom || bottom >= self.cells.len() {
            return;
        }

        let count = usize::min(count as usize, bottom - row + 1);
        for _ in 0..count {
            self.cells.remove(bottom);
            self.cells.insert(
                row,
                blank_row_with_style(self.size.cols, self.current_style),
            );
        }
        self.pending_wrap = false;
    }

    fn delete_lines(&mut self, count: u16) {
        if self.cells.is_empty() || self.size.rows == 0 {
            return;
        }

        let row = self.cursor_row as usize;
        let top = self.scroll_top as usize;
        let bottom = self.scroll_bottom as usize;
        if row < top || row > bottom || bottom >= self.cells.len() {
            return;
        }

        let count = usize::min(count as usize, bottom - row + 1);
        for _ in 0..count {
            self.cells.remove(row);
            self.cells.insert(
                bottom,
                blank_row_with_style(self.size.cols, self.current_style),
            );
        }
        self.pending_wrap = false;
    }

    fn carriage_return(&mut self) {
        self.cursor_col = 0;
        self.pending_wrap = false;
    }

    fn line_feed(&mut self) {
        if self.size.rows == 0 {
            return;
        }
        self.pending_wrap = false;

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
        self.pending_wrap = false;

        if self.cursor_row <= self.scroll_top {
            self.scroll_down_in_region(1);
            self.cursor_row = self.scroll_top;
        } else {
            self.cursor_row -= 1;
        }
    }

    fn backspace(&mut self) {
        self.cursor_col = self.cursor_col.saturating_sub(1);
        self.pending_wrap = false;
    }

    fn tab(&mut self) {
        if self.size.cols == 0 {
            return;
        }

        let next_tab_stop = ((self.cursor_col / 8) + 1) * 8;
        self.cursor_col = next_tab_stop.min(self.size.cols.saturating_sub(1));
        self.pending_wrap = false;
    }

    fn move_cursor_to(&mut self, row: u16, col: u16, origin: bool) {
        self.cursor_row = self.clamp_origin_row(row, origin);
        self.cursor_col = col.min(self.size.cols.saturating_sub(1));
        self.pending_wrap = false;
    }

    fn move_cursor_row(&mut self, row: u16, origin: bool) {
        self.cursor_row = self.clamp_origin_row(row, origin);
        self.pending_wrap = false;
    }

    fn move_cursor_col(&mut self, col: u16) {
        self.cursor_col = col.min(self.size.cols.saturating_sub(1));
        self.pending_wrap = false;
    }

    /// With origin mode rows are relative to the scroll region top and the
    /// cursor cannot leave the region.
    fn clamp_origin_row(&self, row: u16, origin: bool) -> u16 {
        if origin {
            self.scroll_top
                .saturating_add(row)
                .min(self.scroll_bottom.min(self.size.rows.saturating_sub(1)))
        } else {
            row.min(self.size.rows.saturating_sub(1))
        }
    }

    fn move_cursor_relative(&mut self, row_delta: isize, col_delta: isize) {
        let next_row = (self.cursor_row as isize + row_delta)
            .clamp(0, self.size.rows.saturating_sub(1) as isize) as u16;
        let next_col = (self.cursor_col as isize + col_delta)
            .clamp(0, self.size.cols.saturating_sub(1) as isize) as u16;
        self.cursor_row = next_row;
        self.cursor_col = next_col;
        self.pending_wrap = false;
    }

    fn save_cursor(&mut self) {
        self.saved_cursor = SavedCursorState {
            row: self.cursor_row,
            col: self.cursor_col,
            style: self.current_style,
            valid: true,
        };
    }

    fn restore_cursor(&mut self) {
        if !self.saved_cursor.valid {
            return;
        }

        self.cursor_row = self.saved_cursor.row.min(self.size.rows.saturating_sub(1));
        self.cursor_col = self.saved_cursor.col.min(self.size.cols.saturating_sub(1));
        self.current_style = self.saved_cursor.style;
        self.pending_wrap = false;
    }

    fn set_scroll_region(&mut self, top: u16, bottom: u16, origin: bool) {
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
        // DECSTBM homes the cursor; with origin mode home is the region top.
        self.cursor_row = if origin { top } else { 0 };
        self.cursor_col = 0;
        self.pending_wrap = false;
    }

    fn reset_scroll_region(&mut self, origin: bool) {
        self.set_scroll_region(0, self.size.rows.saturating_sub(1), origin);
    }

    fn apply_sgr(&mut self, params: &[SgrSegment]) {
        if params.is_empty() {
            self.current_style = TextStyle::default();
            return;
        }

        let mut index = 0;
        while index < params.len() {
            let segment = &params[index];
            match segment.code {
                0 => self.current_style = TextStyle::default(),
                1 => self.current_style.bold = true,
                2 => self.current_style.dim = true,
                3 => self.current_style.italic = true,
                4 => match segment.subparams.first() {
                    Some(0) => self.current_style.underline = false,
                    Some(1..=5) | None => self.current_style.underline = true,
                    Some(_) => {}
                },
                5 => self.current_style.blink = true,
                7 => self.current_style.inverse = true,
                9 => self.current_style.strikethrough = true,
                22 => {
                    self.current_style.bold = false;
                    self.current_style.dim = false;
                }
                23 => self.current_style.italic = false,
                24 => self.current_style.underline = false,
                25 => self.current_style.blink = false,
                27 => self.current_style.inverse = false,
                29 => self.current_style.strikethrough = false,
                30..=37 => {
                    self.current_style.foreground =
                        Some(ColorValue::Indexed((segment.code - 30) as u8));
                }
                39 => self.current_style.foreground = None,
                40..=47 => {
                    self.current_style.background =
                        Some(ColorValue::Indexed((segment.code - 40) as u8));
                }
                49 => self.current_style.background = None,
                90..=97 => {
                    self.current_style.foreground =
                        Some(ColorValue::Indexed((segment.code - 90 + 8) as u8));
                }
                100..=107 => {
                    self.current_style.background =
                        Some(ColorValue::Indexed((segment.code - 100 + 8) as u8));
                }
                38 | 48 => {
                    let target_foreground = segment.code == 38;
                    let color = if !segment.subparams.is_empty() {
                        Self::colon_color(&segment.subparams)
                    } else {
                        self.semicolon_color(params, &mut index)
                    };
                    if let Some(color) = color {
                        if target_foreground {
                            self.current_style.foreground = Some(color);
                        } else {
                            self.current_style.background = Some(color);
                        }
                    }
                }
                58 => {
                    // Underline color is parsed and discarded.
                    if segment.subparams.is_empty() {
                        match params.get(index + 1).map(|next| next.code) {
                            Some(5) => index += 2,
                            Some(2) => index += 4,
                            _ => {}
                        }
                    }
                }
                _ => {}
            }

            index += 1;
        }
    }

    /// Colon form: `38:5:n`, `38:2:r:g:b`, or `38:2:colorspace:r:g:b`.
    fn colon_color(subparams: &[u16]) -> Option<ColorValue> {
        match subparams[0] {
            5 => subparams
                .get(1)
                .map(|value| ColorValue::Indexed((*value).min(255) as u8)),
            2 => {
                let offset = if subparams.len() >= 5 { 2 } else { 1 };
                match (
                    subparams.get(offset),
                    subparams.get(offset + 1),
                    subparams.get(offset + 2),
                ) {
                    (Some(red), Some(green), Some(blue)) => Some(ColorValue::Rgb(
                        (*red).min(255) as u8,
                        (*green).min(255) as u8,
                        (*blue).min(255) as u8,
                    )),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// Semicolon form: `38;5;n` or `38;2;r:g:b` operands live in the following
    /// segments, which are consumed here.
    fn semicolon_color(&mut self, params: &[SgrSegment], index: &mut usize) -> Option<ColorValue> {
        match params.get(*index + 1).map(|next| next.code) {
            Some(5) => {
                let color = params
                    .get(*index + 2)
                    .map(|value| ColorValue::Indexed(value.code.min(255) as u8));
                if color.is_some() {
                    *index += 2;
                }
                color
            }
            Some(2) => {
                let color = match (
                    params.get(*index + 2),
                    params.get(*index + 3),
                    params.get(*index + 4),
                ) {
                    (Some(red), Some(green), Some(blue)) => Some(ColorValue::Rgb(
                        red.code.min(255) as u8,
                        green.code.min(255) as u8,
                        blue.code.min(255) as u8,
                    )),
                    _ => None,
                };
                if color.is_some() {
                    *index += 4;
                }
                color
            }
            _ => None,
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
                self.styled_scrollback
                    .push_back(render_styled_row(&removed));
                while self.styled_scrollback.len() > MAX_SCROLLBACK_LINES {
                    self.styled_scrollback.pop_front();
                    // Keep the emitted cursor aligned with the trimmed queue so
                    // drain_scrollback_lines never reports stale indices.
                    self.scrollback_emitted_count = self.scrollback_emitted_count.saturating_sub(1);
                }
            }
            self.cells.insert(
                bottom,
                blank_row_with_style(self.size.cols, self.current_style),
            );
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
            self.cells.insert(
                top,
                blank_row_with_style(self.size.cols, self.current_style),
            );
        }
    }

    fn clear_screen(&mut self, mode: u16) {
        if self.cells.is_empty() || self.size.rows == 0 {
            return;
        }
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
                self.pending_wrap = false;
            }
        }
    }

    fn clear_line(&mut self, mode: u16) {
        if self.cells.is_empty() || self.size.rows == 0 {
            return;
        }
        let row = self.cursor_row as usize;
        if row >= self.cells.len() {
            return;
        }
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

    fn delete_chars(&mut self, count: u16) {
        if self.cells.is_empty() || self.size.rows == 0 || self.size.cols == 0 {
            return;
        }

        let row = self.cursor_row as usize;
        if row >= self.cells.len() {
            return;
        }

        let start = self.cursor_col as usize;
        if start >= self.cells[row].len() {
            return;
        }

        let count = usize::max(1, count as usize).min(self.cells[row].len() - start);
        let row_cells = &mut self.cells[row];
        let fill_start = row_cells.len() - count;
        row_cells.copy_within(start + count.., start);
        for cell in &mut row_cells[fill_start..] {
            *cell = blank_cell_with_style(self.current_style);
        }

        self.pending_wrap = false;
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
