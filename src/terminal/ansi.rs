use super::types::{ColorValue, ScreenCell, TerminalSize, TextStyle, WIDE_CONTINUATION};

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

/// Parse a single ANSI-styled line into runs of `(text, style)`.
///
/// Only SGR (`ESC [ ... m`) sequences are interpreted; other escape sequences
/// are dropped.  The supported attributes mirror those produced by
/// `TextStyle::to_ansi` plus the common 256-color/truecolor forms.
pub(crate) fn parse_ansi_styled_line(line: &str) -> Vec<(String, TextStyle)> {
    let mut spans = Vec::new();
    let mut current_text = String::new();
    let mut style = TextStyle::default();

    let mut chars = line.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\x1b' {
            current_text.push(ch);
            continue;
        }

        if chars.peek() != Some(&'[') {
            // Not a CSI sequence; ignore the stray escape.
            continue;
        }
        chars.next();

        let mut params = String::new();
        let mut letter = None;
        while let Some(&c) = chars.peek() {
            if c.is_ascii_alphabetic() {
                letter = Some(c);
                chars.next();
                break;
            }
            params.push(c);
            chars.next();
        }

        if letter != Some('m') {
            // Non-SGR CSI sequences are ignored.
            continue;
        }

        let segments = parse_sgr_segments(&params);
        let new_style = apply_sgr_segments(style, &segments);
        if new_style != style {
            flush_styled_span(&mut spans, &mut current_text, &style);
            style = new_style;
        }
    }

    flush_styled_span(&mut spans, &mut current_text, &style);
    spans
}

fn flush_styled_span(spans: &mut Vec<(String, TextStyle)>, text: &mut String, style: &TextStyle) {
    if !text.is_empty() {
        spans.push((std::mem::take(text), *style));
    }
}

fn apply_sgr_segments(style: TextStyle, segments: &[SgrSegment]) -> TextStyle {
    let mut style = style;
    let mut i = 0;
    while i < segments.len() {
        let segment = &segments[i];
        i += 1;
        match segment.code {
            0 => style = TextStyle::default(),
            1 => style.bold = true,
            2 => style.dim = true,
            3 => style.italic = true,
            4 => style.underline = true,
            5 => style.blink = true,
            7 => style.inverse = true,
            9 => style.strikethrough = true,
            22 => {
                style.bold = false;
                style.dim = false;
            }
            23 => style.italic = false,
            24 => style.underline = false,
            25 => style.blink = false,
            27 => style.inverse = false,
            29 => style.strikethrough = false,
            30..=37 => style.foreground = Some(ColorValue::Indexed(segment.code as u8 - 30)),
            38 => {
                if let Some((color, next)) = parse_sgr_color(segments, i - 1) {
                    style.foreground = Some(color);
                    i = next;
                }
            }
            39 => style.foreground = None,
            40..=47 => style.background = Some(ColorValue::Indexed(segment.code as u8 - 40)),
            48 => {
                if let Some((color, next)) = parse_sgr_color(segments, i - 1) {
                    style.background = Some(color);
                    i = next;
                }
            }
            49 => style.background = None,
            90..=97 => style.foreground = Some(ColorValue::Indexed(segment.code as u8 - 82)),
            100..=107 => style.background = Some(ColorValue::Indexed(segment.code as u8 - 92)),
            _ => {}
        }
    }
    style
}

fn parse_sgr_color(segments: &[SgrSegment], start: usize) -> Option<(ColorValue, usize)> {
    let segment = segments.get(start)?;

    // Colon-separated form (ISO 8613-6), e.g. 38:2::r:g:b or 38:5:n.
    if !segment.subparams.is_empty() {
        match segment.subparams[0] {
            5 if segment.subparams.len() >= 2 => {
                return Some((ColorValue::Indexed(segment.subparams[1] as u8), start + 1));
            }
            2 if segment.subparams.len() >= 4 => {
                let len = segment.subparams.len();
                let r = segment.subparams[len - 3] as u8;
                let g = segment.subparams[len - 2] as u8;
                let b = segment.subparams[len - 1] as u8;
                return Some((ColorValue::Rgb(r, g, b), start + 1));
            }
            _ => {}
        }
    }

    // Semicolon-separated form, e.g. 38;5;n or 38;2;r;g;b.
    let mode = segments.get(start + 1)?.code;
    match mode {
        5 => {
            let index = segments.get(start + 2)?.code as u8;
            Some((ColorValue::Indexed(index), start + 3))
        }
        2 => {
            let r = segments.get(start + 2)?.code as u8;
            let g = segments.get(start + 3)?.code as u8;
            let b = segments.get(start + 4)?.code as u8;
            Some((ColorValue::Rgb(r, g, b), start + 5))
        }
        _ => None,
    }
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

/// One `;`-separated SGR parameter with any `:`-separated subparameters kept
/// intact so forms like `4:3` or `38:2:r:g:b` are not flattened into
/// independent attributes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SgrSegment {
    pub(crate) code: u16,
    pub(crate) subparams: Vec<u16>,
}

pub(crate) fn parse_sgr_segments(params: &str) -> Vec<SgrSegment> {
    if params.is_empty() {
        return Vec::new();
    }

    params
        .split(';')
        .map(|segment| {
            let mut parts = segment.split(':');
            let code = parts
                .next()
                .and_then(|value| value.parse::<u16>().ok())
                .unwrap_or(0);
            let subparams = parts
                .map(|value| value.parse::<u16>().unwrap_or(0))
                .collect();
            SgrSegment { code, subparams }
        })
        .collect()
}

/// Translates a character through the DEC special graphics charset (designated
/// with `ESC ( 0`). Bytes without a graphics mapping pass through unchanged.
pub(crate) fn translate_dec_graphics(ch: char) -> char {
    match ch {
        '`' => '◆',
        'a' => '▒',
        // Control pictures for HT/FF/CR/LF/NL/VT render as blanks.
        'b'..='e' | 'h' | 'i' => ' ',
        'f' => '°',
        'g' => '±',
        'j' => '┘',
        'k' => '┐',
        'l' => '┌',
        'm' => '└',
        'n' => '┼',
        'o' => '⎺',
        'p' => '⎻',
        'q' => '─',
        'r' => '⎼',
        's' => '⎽',
        't' => '├',
        'u' => '┤',
        'v' => '┴',
        'w' => '┬',
        'x' => '│',
        'y' => '≤',
        'z' => '≥',
        '{' => 'π',
        '|' => '≠',
        '}' => '£',
        '~' => '·',
        '_' => '\u{a0}',
        _ => ch,
    }
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
