//! Engine display mode for the remote main-slot viewer: remote output bytes
//! feed a `TerminalEngine` sized to the negotiated remote geometry, and this
//! renderer draws the engine screen into the full-size local pane (top-left
//! anchored, absolute cursor addressing, autowrap disabled) using a
//! dirty-line diff so each frame costs O(changed rows).

use super::remote_main_slot_pane_runtime::{next_ansi_escape_len, terminal_char_display_width};
use crate::terminal::{MouseEncoding, MouseReportingMode, ScreenSnapshot, TerminalSize};

/// Display mode for the remote main-slot viewer, selected once at startup via
/// `WAITAGENT_REMOTE_RENDER` (`engine`|`raw`, default `engine`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RemoteRenderMode {
    Raw,
    Engine,
}

impl RemoteRenderMode {
    pub(crate) fn from_env() -> Self {
        match std::env::var("WAITAGENT_REMOTE_RENDER").as_deref() {
            Ok("raw") => Self::Raw,
            _ => Self::Engine,
        }
    }

    pub(crate) fn is_engine(self) -> bool {
        matches!(self, Self::Engine)
    }
}

/// Terminal modes the renderer proxies to the local pane tty so engine mode
/// gets the capabilities raw passthrough gets for free: bracketed paste and
/// mouse reporting. Cursor visibility (`?25`) is never proxied (the overlay
/// is the cursor) and application cursor keys (`?1`) are handled on the input
/// side instead.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ProxiedModes {
    pub(crate) bracketed_paste: bool,
    pub(crate) mouse_reporting: MouseReportingMode,
    pub(crate) mouse_encoding: MouseEncoding,
}

/// Owns the frame-to-frame state of the engine display path: the previously
/// rendered frame for diffing, a pending full-redraw flag, the geometry the
/// letterbox area was last cleared for, and the terminal modes already
/// proxied to the local pane tty.
pub(crate) struct EngineFrameRenderer {
    prev_frame: Option<ScreenSnapshot>,
    full_redraw_pending: bool,
    letterbox_cleared_for: Option<(TerminalSize, TerminalSize)>,
    proxied_modes: Option<ProxiedModes>,
}

impl EngineFrameRenderer {
    pub(crate) fn new() -> Self {
        Self {
            prev_frame: None,
            full_redraw_pending: false,
            letterbox_cleared_for: None,
            proxied_modes: None,
        }
    }

    /// Force the next frame to clear the whole pane (letterbox included) and
    /// redraw every row. Required after activation, geometry resync, and pane
    /// resize, where content outside the tracked diff area may be stale.
    pub(crate) fn request_full_redraw(&mut self) {
        self.full_redraw_pending = true;
    }

    pub(crate) fn render_frame(
        &mut self,
        next: ScreenSnapshot,
        pane_size: TerminalSize,
        proxied_modes: ProxiedModes,
    ) -> Vec<u8> {
        let mut frame = proxied_mode_transitions(self.proxied_modes, proxied_modes);
        self.proxied_modes = Some(proxied_modes);
        let geometry = (pane_size, next.size);
        let full_redraw = self.full_redraw_pending
            || self.prev_frame.is_none()
            || self.letterbox_cleared_for != Some(geometry);
        frame.extend_from_slice(&diff_frame_ansi(
            if full_redraw {
                None
            } else {
                self.prev_frame.as_ref()
            },
            &next,
            pane_size,
        ));
        self.prev_frame = Some(next);
        self.full_redraw_pending = false;
        if full_redraw {
            self.letterbox_cleared_for = Some(geometry);
        }
        frame
    }
}

/// DECSET/DECRST bytes that move the local pane tty from `prev` to `next`
/// proxied modes. Emitted once per transition, never per frame.
fn proxied_mode_transitions(prev: Option<ProxiedModes>, next: ProxiedModes) -> Vec<u8> {
    let prev = prev.unwrap_or_default();
    let mut out = Vec::new();

    if prev.bracketed_paste != next.bracketed_paste {
        out.extend_from_slice(if next.bracketed_paste {
            b"\x1b[?2004h".as_slice()
        } else {
            b"\x1b[?2004l".as_slice()
        });
    }

    if prev.mouse_reporting != next.mouse_reporting {
        if let Some(code) = mouse_reporting_mode_code(prev.mouse_reporting) {
            out.extend_from_slice(format!("\x1b[?{code}l").as_bytes());
        }
        if let Some(code) = mouse_reporting_mode_code(next.mouse_reporting) {
            out.extend_from_slice(format!("\x1b[?{code}h").as_bytes());
        }
        // When reporting stops entirely, drop the encoding modes locally too.
        if next.mouse_reporting == MouseReportingMode::None {
            out.extend_from_slice(b"\x1b[?1006l\x1b[?1015l".as_slice());
        }
    }

    if prev.mouse_encoding != next.mouse_encoding {
        if let Some(code) = mouse_encoding_mode_code(prev.mouse_encoding) {
            out.extend_from_slice(format!("\x1b[?{code}l").as_bytes());
        }
        if let Some(code) = mouse_encoding_mode_code(next.mouse_encoding) {
            out.extend_from_slice(format!("\x1b[?{code}h").as_bytes());
        }
    }

    out
}

fn mouse_reporting_mode_code(mode: MouseReportingMode) -> Option<u16> {
    match mode {
        MouseReportingMode::None => None,
        MouseReportingMode::Click => Some(1000),
        MouseReportingMode::Drag => Some(1002),
        MouseReportingMode::AnyMotion => Some(1003),
    }
}

fn mouse_encoding_mode_code(encoding: MouseEncoding) -> Option<u16> {
    match encoding {
        MouseEncoding::X10 => None,
        MouseEncoding::Utf8 => Some(1015),
        MouseEncoding::Sgr => Some(1006),
    }
}

/// Emit the ANSI bytes that turn `prev` into `next` on a dumb full-size pane:
/// autowrap stays disabled, a first/full frame clears the screen, every
/// changed row is redrawn with an absolute CUP, and the cursor is painted as
/// a reverse-video overlay (tmux only shows the hardware cursor in the
/// focused pane). Rows and columns beyond the pane bounds are clipped.
pub(crate) fn diff_frame_ansi(
    prev: Option<&ScreenSnapshot>,
    next: &ScreenSnapshot,
    pane_size: TerminalSize,
) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"\x1b[?7l");
    let full_redraw = prev.is_none();
    if full_redraw {
        out.extend_from_slice(b"\x1b[2J");
    }

    let pane_rows = usize::from(pane_size.rows);
    let pane_cols = usize::from(pane_size.cols);
    if pane_rows == 0 || pane_cols == 0 {
        return out;
    }

    let cursor_row = usize::from(next.cursor_row);
    let cursor_col = usize::from(next.cursor_col);
    let cursor_changed = prev
        .map(|prev| {
            (prev.cursor_row, prev.cursor_col, prev.cursor_visible)
                != (next.cursor_row, next.cursor_col, next.cursor_visible)
        })
        .unwrap_or(true);

    for row in 0..next.lines.len().min(pane_rows) {
        let mut changed = full_redraw
            || prev.is_none_or(|prev| {
                prev.lines.get(row) != next.lines.get(row)
                    || prev.styled_lines.get(row) != next.styled_lines.get(row)
            });
        // A moved or visibility-toggled cursor invalidates its old and new
        // rows so the overlay cell is restored/redrawn even when the row
        // content itself is unchanged.
        if !changed && cursor_changed {
            changed =
                row == cursor_row || prev.is_some_and(|prev| usize::from(prev.cursor_row) == row);
        }
        if !changed {
            continue;
        }
        let styled = next.styled_lines.get(row).map(String::as_str).unwrap_or("");
        let line = clip_styled_row(styled, pane_cols);
        out.extend_from_slice(format!("\x1b[{};1H\x1b[K{}", row + 1, line).as_bytes());
    }

    if next.cursor_visible && cursor_row < pane_rows && cursor_col < pane_cols {
        let plain = next.lines.get(cursor_row).map(String::as_str).unwrap_or("");
        let cursor_char = cell_char_at_col(plain, cursor_col);
        out.extend_from_slice(
            format!(
                "\x1b[{};{}H\x1b[7m{}\x1b[27m",
                cursor_row + 1,
                cursor_col + 1,
                cursor_char
            )
            .as_bytes(),
        );
    }

    out
}

/// Clip a styled row to `max_cols` display columns, copying escape sequences
/// verbatim. Trailing padding is dropped (the row is cleared before content
/// is written) and the clip always ends in a reset when styles were emitted.
fn clip_styled_row(styled: &str, max_cols: usize) -> String {
    let mut rendered = String::new();
    let mut index = 0;
    let mut consumed = 0usize;
    let mut saw_escape = false;

    while index < styled.len() && consumed < max_cols {
        let remaining = &styled[index..];
        if remaining.as_bytes().first() == Some(&0x1b) {
            let escape_len = next_ansi_escape_len(remaining);
            rendered.push_str(&remaining[..escape_len]);
            index += escape_len;
            saw_escape = true;
            continue;
        }

        let Some(ch) = remaining.chars().next() else {
            break;
        };
        let width = terminal_char_display_width(ch);
        if width == 0 || consumed + width > max_cols {
            break;
        }
        rendered.push(ch);
        consumed += width;
        index += ch.len_utf8();
    }

    let mut rendered = rendered.trim_end_matches(' ').to_string();
    if saw_escape && !rendered.ends_with("\x1b[0m") {
        rendered.push_str("\x1b[0m");
    }
    rendered
}

/// Character displayed at a 0-based display column of a plain row; blank when
/// the row is shorter than the column.
fn cell_char_at_col(line: &str, col: usize) -> char {
    let mut display_col = 0usize;
    for ch in line.chars() {
        let width = terminal_char_display_width(ch);
        if display_col == col || display_col + width > col {
            return ch;
        }
        display_col += width;
    }
    ' '
}

#[cfg(test)]
mod tests {
    use super::{diff_frame_ansi, EngineFrameRenderer, ProxiedModes};
    use crate::terminal::{MouseEncoding, MouseReportingMode};
    use crate::terminal::{ScreenSnapshot, TerminalEngine, TerminalSize};

    fn size(cols: u16, rows: u16) -> TerminalSize {
        TerminalSize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        }
    }

    fn snapshot_for(cols: u16, rows: u16, feed: &[u8]) -> ScreenSnapshot {
        let mut engine = TerminalEngine::new(size(cols, rows));
        engine.feed(feed);
        engine.snapshot_visible()
    }

    fn frame_text(frame: &[u8]) -> String {
        String::from_utf8_lossy(frame).to_string()
    }

    fn emitted_row_count(frame: &[u8]) -> usize {
        frame_text(frame).matches("\x1b[K").count()
    }

    /// Visible text of a frame segment with CSI/OSC escapes stripped.
    fn visible_text(segment: &str) -> String {
        let mut visible = String::new();
        let mut chars = segment.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch != '\x1b' {
                visible.push(ch);
                continue;
            }
            match chars.next() {
                Some('[') => {
                    for next in chars.by_ref() {
                        if ('\x40'..='\x7e').contains(&next) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    let mut prev = '\0';
                    for next in chars.by_ref() {
                        if next == '\x07' || (prev == '\x1b' && next == '\\') {
                            break;
                        }
                        prev = next;
                    }
                }
                _ => {}
            }
        }
        visible
    }

    /// Visible width of each row body emitted as `\x1b[row;1H\x1b[K<body>`.
    fn emitted_row_widths(frame: &[u8]) -> Vec<usize> {
        frame_text(frame)
            .split("\x1b[K")
            .skip(1)
            .map(|body| {
                let body = body.split('\x1b').next().unwrap_or("");
                visible_text(body).chars().count()
            })
            .collect()
    }

    #[test]
    fn first_frame_clears_and_draws_every_row() {
        let next = snapshot_for(8, 3, b"ab\r\ncd");
        let frame = frame_text(&diff_frame_ansi(None, &next, size(8, 3)));

        assert!(frame.starts_with("\x1b[?7l\x1b[2J"));
        assert!(frame.contains("\x1b[1;1H\x1b[Kab"));
        assert!(frame.contains("\x1b[2;1H\x1b[Kcd"));
        assert!(frame.contains("\x1b[3;1H\x1b[K"));
        assert_eq!(emitted_row_count(frame.as_bytes()), 3);
    }

    #[test]
    fn unchanged_rows_are_skipped_on_diff_frames() {
        let prev = snapshot_for(8, 3, b"ab\r\ncd");
        let next = snapshot_for(8, 3, b"ab\r\ncd");
        let frame = frame_text(&diff_frame_ansi(Some(&prev), &next, size(8, 3)));

        assert!(!frame.contains("\x1b[2J"));
        assert_eq!(emitted_row_count(frame.as_bytes()), 0);
    }

    #[test]
    fn one_cell_change_emits_exactly_one_row() {
        let prev = snapshot_for(8, 3, b"abcd");
        let next = snapshot_for(8, 3, b"abcd\x1b[1;2HZ");
        let frame = frame_text(&diff_frame_ansi(Some(&prev), &next, size(8, 3)));

        assert_eq!(emitted_row_count(frame.as_bytes()), 1);
        assert!(frame.contains("\x1b[1;1H\x1b[KaZcd"));
    }

    #[test]
    fn cursor_overlay_is_reverse_video_at_the_cursor_cell() {
        let prev = snapshot_for(8, 3, b"abcd");
        let next = snapshot_for(8, 3, b"abcd\x1b[2;3H");
        let frame = frame_text(&diff_frame_ansi(Some(&prev), &next, size(8, 3)));

        assert!(
            frame.contains("\x1b[2;3H\x1b[7m \x1b[27m"),
            "expected reverse-video blank at row 2 col 3: {frame:?}"
        );
        // The previous cursor cell must be restored by re-emitting its row.
        assert!(frame.contains("\x1b[1;1H\x1b[Kabcd"));
    }

    #[test]
    fn hidden_cursor_emits_no_overlay_and_restores_previous_cell() {
        let prev = snapshot_for(8, 3, b"abcd");
        let next = snapshot_for(8, 3, b"abcd\x1b[?25l");
        let frame = frame_text(&diff_frame_ansi(Some(&prev), &next, size(8, 3)));

        assert!(!frame.contains("\x1b[7m"));
        assert!(frame.contains("\x1b[1;1H\x1b[Kabcd"));
    }

    #[test]
    fn letterbox_rows_are_cleared_on_full_redraw_only() {
        let prev = snapshot_for(10, 2, b"ab");
        let pane = size(40, 10);
        let full = frame_text(&diff_frame_ansi(None, &prev, pane));

        assert!(full.contains("\x1b[2J"));
        assert!(full.contains("\x1b[1;1H"));
        assert!(full.contains("\x1b[2;1H"));
        assert!(!full.contains("\x1b[3;1H"));

        let next = snapshot_for(10, 2, b"abX");
        let diff = frame_text(&diff_frame_ansi(Some(&prev), &next, pane));

        assert!(!diff.contains("\x1b[2J"));
        assert!(diff.contains("\x1b[1;1H\x1b[K"));
        assert!(!diff.contains("\x1b[2;1H"));
        assert!(!diff.contains("\x1b[3;1H"));
    }

    #[test]
    fn screen_larger_than_pane_is_clipped_to_pane_bounds() {
        let wide = snapshot_for(
            30,
            4,
            b"012345678901234567890123456789\r\nrow2\r\nrow3\r\nrow4",
        );
        let frame = frame_text(&diff_frame_ansi(None, &wide, size(10, 2)));

        assert!(frame.contains("\x1b[1;1H"));
        assert!(frame.contains("\x1b[2;1H"));
        assert!(!frame.contains("\x1b[3;1H"));
        for width in emitted_row_widths(frame.as_bytes()) {
            assert!(width <= 10, "emitted row exceeds pane width: {width}");
        }
    }

    #[test]
    fn renderer_diffs_after_first_frame_and_honours_full_redraw_requests() {
        let mut renderer = EngineFrameRenderer::new();
        let pane = size(8, 3);

        let first = frame_text(&renderer.render_frame(
            snapshot_for(8, 3, b"ab"),
            pane,
            ProxiedModes::default(),
        ));
        assert!(first.contains("\x1b[2J"));

        let second = frame_text(&renderer.render_frame(
            snapshot_for(8, 3, b"ab"),
            pane,
            ProxiedModes::default(),
        ));
        assert!(!second.contains("\x1b[2J"));
        assert_eq!(emitted_row_count(second.as_bytes()), 0);

        renderer.request_full_redraw();
        let third = frame_text(&renderer.render_frame(
            snapshot_for(8, 3, b"ab"),
            pane,
            ProxiedModes::default(),
        ));
        assert!(third.contains("\x1b[2J"));
        assert_eq!(emitted_row_count(third.as_bytes()), 3);
    }

    #[test]
    fn renderer_forces_full_redraw_when_pane_or_screen_geometry_changes() {
        let mut renderer = EngineFrameRenderer::new();

        let first = frame_text(&renderer.render_frame(
            snapshot_for(8, 3, b"ab"),
            size(8, 3),
            ProxiedModes::default(),
        ));
        assert!(first.contains("\x1b[2J"));

        let resized_pane = frame_text(&renderer.render_frame(
            snapshot_for(8, 3, b"ab"),
            size(9, 3),
            ProxiedModes::default(),
        ));
        assert!(resized_pane.contains("\x1b[2J"));

        let resized_screen = frame_text(&renderer.render_frame(
            snapshot_for(9, 3, b"ab"),
            size(9, 3),
            ProxiedModes::default(),
        ));
        assert!(resized_screen.contains("\x1b[2J"));
    }

    #[test]
    fn wrapped_long_line_never_renders_rows_wider_than_remote_geometry() {
        // The original bug shape: a 45-char line wrapped by the remote at
        // T=20 must render as rows of at most 20 columns even though the
        // local pane is much wider.
        let mut engine = TerminalEngine::new(size(20, 4));
        engine.feed(b"abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHI");
        let next = engine.snapshot_visible();

        let mut renderer = EngineFrameRenderer::new();
        let frame = renderer.render_frame(next, size(60, 10), ProxiedModes::default());
        let widths = emitted_row_widths(&frame);

        assert_eq!(widths.len(), 4);
        for width in widths {
            assert!(width <= 20, "rendered row exceeds remote width: {width}");
        }
        let text = frame_text(&frame);
        assert!(text.contains("\x1b[1;1H\x1b[Kabcdefghijklmnopqrst"));
        assert!(text.contains("\x1b[2;1H\x1b[Kuvwxyz0123456789ABCD"));
        assert!(text.contains("\x1b[3;1H\x1b[KEFGHI"));
    }

    #[test]
    fn renderer_proxies_bracketed_paste_transitions_once() {
        let mut renderer = EngineFrameRenderer::new();
        let modes_on = ProxiedModes {
            bracketed_paste: true,
            ..ProxiedModes::default()
        };

        let first =
            frame_text(&renderer.render_frame(snapshot_for(8, 3, b"ab"), size(8, 3), modes_on));
        assert!(first.contains("\x1b[?2004h"));

        let second =
            frame_text(&renderer.render_frame(snapshot_for(8, 3, b"ab"), size(8, 3), modes_on));
        assert!(!second.contains("2004"));

        let off = frame_text(&renderer.render_frame(
            snapshot_for(8, 3, b"ab"),
            size(8, 3),
            ProxiedModes::default(),
        ));
        assert!(off.contains("\x1b[?2004l"));
    }

    #[test]
    fn renderer_proxies_mouse_modes_with_requested_encoding() {
        let mut renderer = EngineFrameRenderer::new();
        let sgr_click = ProxiedModes {
            bracketed_paste: false,
            mouse_reporting: MouseReportingMode::Click,
            mouse_encoding: MouseEncoding::Sgr,
        };

        let first =
            frame_text(&renderer.render_frame(snapshot_for(8, 3, b"ab"), size(8, 3), sgr_click));
        assert!(first.contains("\x1b[?1000h"));
        assert!(first.contains("\x1b[?1006h"));

        // Switching reporting level resets the old mode and keeps encoding.
        let drag = ProxiedModes {
            mouse_reporting: MouseReportingMode::Drag,
            ..sgr_click
        };
        let second =
            frame_text(&renderer.render_frame(snapshot_for(8, 3, b"ab"), size(8, 3), drag));
        assert!(second.contains("\x1b[?1000l"));
        assert!(second.contains("\x1b[?1002h"));
        assert!(!second.contains("1006"));

        // Reporting off resets reporting and encoding modes.
        let off = frame_text(&renderer.render_frame(
            snapshot_for(8, 3, b"ab"),
            size(8, 3),
            ProxiedModes::default(),
        ));
        assert!(off.contains("\x1b[?1002l"));
        assert!(off.contains("\x1b[?1006l"));
        assert!(off.contains("\x1b[?1015l"));
    }

    #[test]
    fn renderer_emits_utf8_mouse_encoding_when_requested() {
        let mut renderer = EngineFrameRenderer::new();
        let utf8 = ProxiedModes {
            bracketed_paste: false,
            mouse_reporting: MouseReportingMode::AnyMotion,
            mouse_encoding: MouseEncoding::Utf8,
        };

        let frame = frame_text(&renderer.render_frame(snapshot_for(8, 3, b"ab"), size(8, 3), utf8));
        assert!(frame.contains("\x1b[?1003h"));
        assert!(frame.contains("\x1b[?1015h"));
    }

    #[test]
    fn engine_resized_to_pane_size_renders_full_content_without_clipping() {
        // Simulate the fixed GeometryResyncDue behavior: authority reports a
        // geometry larger than the local pane, so the engine is clamped to the
        // pane size before rendering. Content must remain visible (not clipped
        // by the pane bounds).
        let mut engine = TerminalEngine::new(size(30, 4));
        engine.feed(b"012345678901234567890123456789\r\nrow2\r\nrow3\r\nrow4");
        // Local pane is only 10x2; clamp the engine before rendering.
        engine.resize(size(10, 2));
        let next = engine.snapshot_visible();

        let frame = frame_text(&diff_frame_ansi(None, &next, size(10, 2)));

        assert!(frame.contains("\x1b[1;1H\x1b[K0123456789"));
        assert!(frame.contains("\x1b[2;1H\x1b[Krow2"));
        assert!(!frame.contains("\x1b[3;1H"));
        for width in emitted_row_widths(frame.as_bytes()) {
            assert!(width <= 10, "emitted row exceeds pane width: {width}");
        }
    }
}
