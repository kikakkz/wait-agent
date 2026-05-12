use super::ansi::{
    blank_cell, blank_cell_with_style, blank_cells, blank_cells_with_style, blank_row_with_style,
    char_display_width, decode_utf8_chars, first_or, parse_csi_numbers, render_plain_row,
    render_styled_row, second_or,
};
use super::types::*;

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
                        b'7' => {
                            self.active_buffer_mut().save_cursor();
                            index += 1;
                        }
                        b'8' => {
                            self.active_buffer_mut().restore_cursor();
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
            'P' => self.active_buffer_mut().delete_chars(first_or(&numbers, 1)),
            'S' => self
                .active_buffer_mut()
                .scroll_up_in_region(first_or(&numbers, 1)),
            'r' => {
                if numbers.is_empty() {
                    self.active_buffer_mut().reset_scroll_region();
                } else {
                    let top = first_or(&numbers, 1).saturating_sub(1);
                    let bottom =
                        second_or(&numbers, self.active_buffer().size.rows).saturating_sub(1);
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
            's' if numbers.is_empty() => self.active_buffer_mut().save_cursor(),
            'u' if numbers.is_empty() => self.active_buffer_mut().restore_cursor(),
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

#[derive(Debug, Clone)]
struct ScreenBuffer {
    size: TerminalSize,
    cells: Vec<Vec<ScreenCell>>,
    cursor_row: u16,
    cursor_col: u16,
    pending_wrap: bool,
    scroll_top: u16,
    scroll_bottom: u16,
    scrollback: Vec<String>,
    styled_scrollback: Vec<String>,
    current_style: TextStyle,
    saved_cursor: SavedCursorState,
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
            scrollback: Vec::new(),
            styled_scrollback: Vec::new(),
            current_style: TextStyle::default(),
            saved_cursor: SavedCursorState::default(),
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
        self.pending_wrap = false;
        self.scroll_top = self.scroll_top.min(size.rows.saturating_sub(1));
        self.scroll_bottom = self
            .scroll_bottom
            .max(self.scroll_top)
            .min(size.rows.saturating_sub(1));
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
        ScreenSnapshot {
            size: self.size,
            lines: self.cells.iter().map(|row| render_plain_row(row)).collect(),
            styled_lines: self
                .cells
                .iter()
                .map(|row| render_styled_row(row))
                .collect(),
            active_style_ansi: self.current_style.to_ansi(),
            scrollback: self.scrollback.clone(),
            styled_scrollback: self.styled_scrollback.clone(),
            scroll_top: self.scroll_top,
            scroll_bottom: self.scroll_bottom,
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

        if self.pending_wrap {
            self.cursor_col = 0;
            self.line_feed();
            self.pending_wrap = false;
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
                if self.cursor_col + 2 >= self.size.cols {
                    self.cursor_col = self.size.cols.saturating_sub(1);
                    self.pending_wrap = true;
                } else {
                    self.cursor_col += 2;
                }
            } else {
                self.cells[row][col + 1] = ScreenCell {
                    ch: WIDE_CONTINUATION,
                    style: self.current_style,
                };
                if self.cursor_col + 2 >= self.size.cols {
                    self.cursor_col = self.size.cols.saturating_sub(1);
                    self.pending_wrap = true;
                } else {
                    self.cursor_col += 2;
                }
            }
        } else {
            if self.cursor_col + 1 >= self.size.cols {
                self.cursor_col = self.size.cols.saturating_sub(1);
                self.pending_wrap = true;
            } else {
                self.cursor_col += 1;
            }
        }
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

    fn move_cursor_to(&mut self, row: u16, col: u16) {
        self.cursor_row = row.min(self.size.rows.saturating_sub(1));
        self.cursor_col = col.min(self.size.cols.saturating_sub(1));
        self.pending_wrap = false;
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
        self.pending_wrap = false;
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
                self.scrollback.push(render_plain_row(&removed));
                self.styled_scrollback.push(render_styled_row(&removed));
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
