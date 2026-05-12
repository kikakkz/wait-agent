use super::types::{ScreenCell, TerminalSize, TextStyle, WIDE_CONTINUATION};

pub(crate) fn decode_utf8_chars(bytes: &[u8], pending_utf8: &mut Vec<u8>) -> Vec<char> {
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

pub(crate) fn char_display_width(ch: char) -> u16 {
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

pub(crate) fn render_plain_row(row: &[ScreenCell]) -> String {
    row.iter()
        .filter(|cell| cell.ch != WIDE_CONTINUATION)
        .map(|cell| cell.ch)
        .collect::<String>()
}

pub(crate) fn render_styled_row(row: &[ScreenCell]) -> String {
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

pub(crate) fn parse_csi_numbers(params: &str) -> Vec<u16> {
    if params.is_empty() {
        return Vec::new();
    }

    params
        .split(';')
        .map(|value| value.parse::<u16>().unwrap_or(0))
        .collect()
}

pub(crate) fn first_or(values: &[u16], default: u16) -> u16 {
    values.first().copied().unwrap_or(default)
}

pub(crate) fn second_or(values: &[u16], default: u16) -> u16 {
    values.get(1).copied().unwrap_or(default)
}

pub(crate) fn blank_cells(size: TerminalSize) -> Vec<Vec<ScreenCell>> {
    blank_cells_with_style(size, TextStyle::default())
}

pub(crate) fn blank_cells_with_style(size: TerminalSize, style: TextStyle) -> Vec<Vec<ScreenCell>> {
    (0..size.rows)
        .map(|_| blank_row_with_style(size.cols, style))
        .collect()
}

pub(crate) fn blank_row(cols: u16) -> Vec<ScreenCell> {
    blank_row_with_style(cols, TextStyle::default())
}

pub(crate) fn blank_row_with_style(cols: u16, style: TextStyle) -> Vec<ScreenCell> {
    vec![blank_cell_with_style(style); cols as usize]
}

pub(crate) fn blank_cell() -> ScreenCell {
    blank_cell_with_style(TextStyle::default())
}

pub(crate) fn blank_cell_with_style(style: TextStyle) -> ScreenCell {
    ScreenCell { ch: ' ', style }
}
