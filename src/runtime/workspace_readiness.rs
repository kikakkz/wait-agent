use crate::terminal::ScreenSnapshot;

const WORKSPACE_STATUS_ROWS: usize = 4;
const FRAME_START: &[u8] = b"\x1b[H";
const ROW_ONE_START: &[u8] = b"\x1b[1;1H";
const CLEAR_TO_LINE_END: &[u8] = b"\x1b[K";

pub(crate) fn workspace_snapshot_ready(snapshot: &ScreenSnapshot) -> bool {
    workspace_snapshot_has_visible_workspace(snapshot) && workspace_snapshot_has_chrome(snapshot)
}

pub(crate) fn attach_frame_has_visible_workspace(bytes: &[u8]) -> bool {
    if looks_like_full_frame(bytes) {
        return full_frame_has_visible_workspace(bytes);
    }

    ansi_visible_text(visible_workspace_main_bytes(bytes))
        .chars()
        .any(|ch| !ch.is_whitespace())
}

pub(crate) fn looks_like_full_frame(bytes: &[u8]) -> bool {
    let Some(frame_start) = full_frame_start_offset(bytes) else {
        return false;
    };

    bytes[frame_start..]
        .windows(ROW_ONE_START.len())
        .any(|window| window == ROW_ONE_START)
}

pub(crate) fn full_frame_has_visible_workspace(bytes: &[u8]) -> bool {
    full_frame_main_lines(bytes)
        .into_iter()
        .any(|visible| line_has_visible_workspace_content(&visible))
}

pub(crate) fn full_frame_has_chrome(bytes: &[u8]) -> bool {
    let mut saw_keys = false;
    let mut saw_status = false;

    for visible in full_frame_main_lines(bytes) {
        let trimmed = visible.trim();
        if trimmed.starts_with("keys:") {
            saw_keys = true;
        } else if trimmed.starts_with("WaitAgent |") {
            saw_status = true;
        }
    }

    saw_keys && saw_status
}

fn workspace_snapshot_has_visible_workspace(snapshot: &ScreenSnapshot) -> bool {
    let work_rows = snapshot.lines.len().saturating_sub(WORKSPACE_STATUS_ROWS);
    snapshot.lines[..work_rows]
        .iter()
        .map(|line| visible_workspace_main_text(line))
        .any(line_has_visible_workspace_content)
}

fn workspace_snapshot_has_chrome(snapshot: &ScreenSnapshot) -> bool {
    let mut saw_keys = false;
    let mut saw_status = false;

    for line in &snapshot.lines {
        let visible = visible_workspace_main_text(line).trim();
        if visible.starts_with("keys:") {
            saw_keys = true;
        } else if visible.starts_with("WaitAgent |") {
            saw_status = true;
        }
    }

    saw_keys && saw_status
}

fn full_frame_start_offset(bytes: &[u8]) -> Option<usize> {
    bytes
        .windows(FRAME_START.len())
        .position(|window| window == FRAME_START)
}

fn visible_workspace_main_text(line: &str) -> &str {
    line.split_once('┃').map(|(main, _)| main).unwrap_or(line)
}

fn visible_workspace_main_bytes(bytes: &[u8]) -> &[u8] {
    let divider = "┃".as_bytes();
    bytes
        .windows(divider.len())
        .position(|window| window == divider)
        .map(|index| &bytes[..index])
        .unwrap_or(bytes)
}

fn full_frame_main_lines(bytes: &[u8]) -> Vec<String> {
    let mut index = 0;
    let mut lines = Vec::new();

    while index < bytes.len() {
        let Some(row_start_offset) = bytes[index..]
            .windows(2)
            .position(|window| window == b"\x1b[")
        else {
            break;
        };
        index += row_start_offset + 2;

        let row_digits_start = index;
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            index += 1;
        }
        if row_digits_start == index || index + 2 >= bytes.len() {
            continue;
        }
        if bytes[index] != b';' || bytes[index + 1] != b'1' || bytes[index + 2] != b'H' {
            continue;
        }
        index += 3;

        let content_start = index;
        let Some(clear_offset) = bytes[index..]
            .windows(CLEAR_TO_LINE_END.len())
            .position(|window| window == CLEAR_TO_LINE_END)
        else {
            break;
        };
        let content_end = index + clear_offset;
        index = content_end + CLEAR_TO_LINE_END.len();

        lines.push(ansi_visible_text(visible_workspace_main_bytes(
            &bytes[content_start..content_end],
        )));
    }

    lines
}

fn line_has_visible_workspace_content(line: &str) -> bool {
    let trimmed = line.trim();
    !trimmed.is_empty()
        && !trimmed.starts_with("keys:")
        && !trimmed.starts_with("WaitAgent |")
        && !trimmed.chars().all(|ch| ch == '━')
}

fn ansi_visible_text(bytes: &[u8]) -> String {
    let mut visible = Vec::new();
    let mut index = 0;

    while index < bytes.len() {
        match bytes[index] {
            0x1b => {
                if index + 1 >= bytes.len() {
                    break;
                }
                match bytes[index + 1] {
                    b'[' => {
                        index += 2;
                        while index < bytes.len() {
                            let byte = bytes[index];
                            index += 1;
                            if (0x40..=0x7e).contains(&byte) {
                                break;
                            }
                        }
                    }
                    b']' => {
                        index += 2;
                        while index < bytes.len() {
                            match bytes[index] {
                                0x07 => {
                                    index += 1;
                                    break;
                                }
                                0x1b if index + 1 < bytes.len() && bytes[index + 1] == b'\\' => {
                                    index += 2;
                                    break;
                                }
                                _ => index += 1,
                            }
                        }
                    }
                    _ => index += 2,
                }
            }
            byte => {
                visible.push(byte);
                index += 1;
            }
        }
    }

    String::from_utf8_lossy(&visible).into_owned()
}

#[cfg(test)]
fn frame_has_visible_first_line(bytes: &[u8]) -> bool {
    let Some(start) = bytes
        .windows(ROW_ONE_START.len())
        .position(|window| window == ROW_ONE_START)
    else {
        return false;
    };
    let content_start = start + ROW_ONE_START.len();
    let content_end = bytes[content_start..]
        .windows(CLEAR_TO_LINE_END.len())
        .position(|window| window == CLEAR_TO_LINE_END)
        .map(|offset| content_start + offset)
        .unwrap_or(bytes.len());
    ansi_visible_text(visible_workspace_main_bytes(
        &bytes[content_start..content_end],
    ))
    .chars()
    .any(|ch| !ch.is_whitespace())
}

#[cfg(test)]
mod tests {
    use super::{
        attach_frame_has_visible_workspace, frame_has_visible_first_line, looks_like_full_frame,
        workspace_snapshot_ready,
    };
    use crate::terminal::{ScreenSnapshot, TerminalSize};

    fn snapshot(lines: &[&str], cursor_row: u16, cursor_col: u16) -> ScreenSnapshot {
        let size = TerminalSize {
            rows: lines.len() as u16,
            cols: lines.iter().map(|line| line.len()).max().unwrap_or(0) as u16,
            pixel_width: 0,
            pixel_height: 0,
        };
        ScreenSnapshot {
            size,
            lines: lines.iter().map(|line| line.to_string()).collect(),
            styled_lines: lines.iter().map(|line| line.to_string()).collect(),
            active_style_ansi: "\x1b[0m".to_string(),
            scrollback: Vec::new(),
            styled_scrollback: Vec::new(),
            scroll_top: 0,
            scroll_bottom: size.rows.saturating_sub(1),
            window_title: None,
            cursor_row,
            cursor_col,
            cursor_visible: true,
            alternate_screen: true,
        }
    }

    #[test]
    fn detects_visible_first_line_in_full_frame_bytes() {
        let bytes = b"\x1b[H\x1b[1;1Hprompt$ \x1b[K\x1b[2;1H\x1b[K";
        assert!(looks_like_full_frame(bytes));
        assert!(frame_has_visible_first_line(bytes));
    }

    #[test]
    fn ignores_blank_first_line_in_full_frame_bytes() {
        let bytes = b"\x1b[H\x1b[1;1H\x1b[K\x1b[2;1H\x1b[K";
        assert!(looks_like_full_frame(bytes));
        assert!(!frame_has_visible_first_line(bytes));
    }

    #[test]
    fn ignores_sidebar_only_first_line_in_full_frame_bytes() {
        let bytes =
            "\x1b[H\x1b[1;1H                                                   ┃ Sessions\x1b[K"
                .as_bytes();
        assert!(looks_like_full_frame(bytes));
        assert!(!frame_has_visible_first_line(bytes));
        assert!(!attach_frame_has_visible_workspace(bytes));
    }

    #[test]
    fn attach_frame_detects_visible_workspace_text_in_non_frame_output() {
        assert!(attach_frame_has_visible_workspace(b"prompt$ "));
        assert!(!attach_frame_has_visible_workspace(
            "   ┃ Sessions".as_bytes()
        ));
    }

    #[test]
    fn attach_frame_detects_visible_workspace_text_below_first_row() {
        let bytes = b"\x1b[H\x1b[1;1H                                                   \x1b[K\x1b[2;1Hprompt$ \x1b[K\x1b[3;1Hkeys: demo\x1b[K";
        assert!(attach_frame_has_visible_workspace(bytes));
    }

    #[test]
    fn full_frame_detection_accepts_leading_terminal_mode_prefixes() {
        let bytes = b"\x1b[?1049h\x1b[H\x1b[?2026h\x1b[H\x1b[1;1Hprompt$ \x1b[K";
        assert!(looks_like_full_frame(bytes));
        assert!(frame_has_visible_first_line(bytes));
        assert!(attach_frame_has_visible_workspace(bytes));
    }

    #[test]
    fn sidebar_only_diff_is_not_treated_as_full_frame() {
        let bytes = "\x1b[?2026h\x1b[H\x1b[3;52H┃\x1b[3;53H> bash@local 🔊I\x1b[K".as_bytes();
        assert!(!looks_like_full_frame(bytes));
        assert!(!attach_frame_has_visible_workspace(bytes));
    }

    #[test]
    fn workspace_snapshot_ready_when_cursor_has_moved() {
        assert!(!workspace_snapshot_ready(&snapshot(
            &["", "", "", "━━━━━━━━", "keys: demo", "WaitAgent | bash",],
            0,
            12,
        )));
    }

    #[test]
    fn workspace_snapshot_ready_when_work_area_has_content() {
        assert!(workspace_snapshot_ready(&snapshot(
            &[
                "prompt line",
                "",
                "",
                "━━━━━━━━",
                "keys: demo",
                "WaitAgent | bash",
            ],
            0,
            0,
        )));
    }

    #[test]
    fn workspace_snapshot_not_ready_for_footer_only_frame() {
        assert!(!workspace_snapshot_ready(&snapshot(
            &["", "", "", "━━━━━━━━", "keys: demo", "WaitAgent | bash",],
            0,
            0,
        )));
    }

    #[test]
    fn workspace_snapshot_not_ready_when_prompt_is_present_without_chrome() {
        assert!(!workspace_snapshot_ready(&snapshot(
            &["prompt line", "", "", "", "", ""],
            0,
            0,
        )));
    }

    #[test]
    fn attach_frame_ignores_footer_only_snapshot_without_prompt() {
        let bytes = concat!(
            "\x1b[H",
            "\x1b[1;1H                                                   ┃ ← back  ↑↓ move  enter swi \x1b[K",
            "\x1b[2;1H                                                   ┃> bash@local             🔊I\x1b[K",
            "\x1b[20;1H━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┃                            \x1b[K",
            "\x1b[21;1Hkeys: ^W cmd  ^B/^F switch  ^N new  ^L picker  ^X c┃                            \x1b[K",
            "\x1b[22;1HWaitAgent | bash | local/session-1                 ┃bash@local | INPUT | unknow \x1b[K",
            "\x1b[23;1HWaitAgent | bash | local/session-1                      active | 1 waiting | 1/1\x1b[K",
            "\x1b[24;1H                                                                                \x1b[K",
        )
        .as_bytes();

        assert!(!attach_frame_has_visible_workspace(bytes));
    }

    #[test]
    fn workspace_snapshot_not_ready_for_sidebar_only_work_rows() {
        assert!(!workspace_snapshot_ready(&snapshot(
            &[
                "                                                   ┃ Sessions",
                "",
                "divider",
                "keys",
                "status",
            ],
            0,
            0,
        )));
    }
}
