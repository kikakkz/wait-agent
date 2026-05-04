const ANSI_RESET: &str = "\x1b[0m";
const ANSI_BG_BAR: &str = "\x1b[48;5;24m\x1b[38;5;255m";
const ANSI_BG_SIDEBAR_HEADER: &str = "\x1b[48;5;236m\x1b[1;38;5;255m";
const ANSI_BG_SIDEBAR_HINT: &str = "\x1b[48;5;235m\x1b[38;5;246m";
const ANSI_BG_SIDEBAR_ITEM: &str = "\x1b[48;5;234m\x1b[38;5;250m";
const ANSI_BG_SIDEBAR_ACTIVE: &str = "\x1b[48;5;240m\x1b[1;38;5;255m";
const ANSI_BG_SIDEBAR_DETAIL: &str = "\x1b[48;5;236m\x1b[38;5;252m";
const ANSI_BG_SIDEBAR_SEPARATOR: &str = "\x1b[48;5;236m\x1b[38;5;244m";
const ANSI_FG_SIDEBAR_RUNNING: &str = "\x1b[38;5;121m";
const ANSI_FG_SIDEBAR_INPUT: &str = "\x1b[38;5;227m";
const ANSI_FG_SIDEBAR_CONFIRM: &str = "\x1b[38;5;215m";
const ANSI_FG_SIDEBAR_UNKNOWN: &str = "\x1b[38;5;244m";

pub const TMUX_MENU_STYLE: &str = "fg=colour250,bg=colour235";
pub const TMUX_MENU_SELECTED_STYLE: &str = "fg=colour255,bg=colour31";
pub const TMUX_MENU_BORDER_STYLE: &str = "fg=colour24,bg=colour235";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SidebarBadgeState {
    Running,
    Input,
    Confirm,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SidebarRowStyle {
    Normal,
    Current,
    Selected,
}

pub fn style_status_line(line: &str, width: usize) -> String {
    format!("{ANSI_BG_BAR}{}{ANSI_RESET}", pad_right(line, width))
}

pub fn style_sidebar_header_line(line: &str, width: usize) -> String {
    format!(
        "{ANSI_BG_SIDEBAR_HEADER}{}{ANSI_RESET}",
        pad_right(line, width)
    )
}

pub fn style_sidebar_hint_line(line: &str, width: usize) -> String {
    format!(
        "{ANSI_BG_SIDEBAR_HINT}{}{ANSI_RESET}",
        pad_right(line, width)
    )
}

pub fn style_sidebar_item_line(line: &str, width: usize, style: SidebarRowStyle) -> String {
    let prefix = sidebar_row_prefix(style);
    format!("{prefix}{}{ANSI_RESET}", pad_right(line, width))
}

pub fn style_sidebar_detail_line(line: &str, width: usize) -> String {
    format!(
        "{ANSI_BG_SIDEBAR_DETAIL}{}{ANSI_RESET}",
        pad_right(line, width)
    )
}

pub fn style_sidebar_separator_line(line: &str, width: usize) -> String {
    format!(
        "{ANSI_BG_SIDEBAR_SEPARATOR}{}{ANSI_RESET}",
        pad_right(line, width)
    )
}

pub fn style_sidebar_badge(
    state: SidebarBadgeState,
    row_style: SidebarRowStyle,
    _now_millis: u128,
) -> (String, usize) {
    let badge = match state {
        SidebarBadgeState::Running => "🔥R",
        SidebarBadgeState::Input => "🔊I",
        SidebarBadgeState::Confirm => "📢C",
        SidebarBadgeState::Unknown => "·U",
    };
    let color = match state {
        SidebarBadgeState::Running => ANSI_FG_SIDEBAR_RUNNING,
        SidebarBadgeState::Input => ANSI_FG_SIDEBAR_INPUT,
        SidebarBadgeState::Confirm => ANSI_FG_SIDEBAR_CONFIRM,
        SidebarBadgeState::Unknown => ANSI_FG_SIDEBAR_UNKNOWN,
    };
    let row_prefix = sidebar_row_prefix(row_style);
    (format!("{color}{badge}{row_prefix}"), display_width(badge))
}

pub fn right_align(text: &str, width: usize) -> String {
    let text = truncate_display_width(text, width);
    let padding = width.saturating_sub(display_width(&text));
    format!("{}{}", " ".repeat(padding), text)
}

pub fn truncate_display_width(text: &str, width: usize) -> String {
    let mut output = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let ch_width = char_width(ch);
        if used + ch_width > width {
            break;
        }
        output.push(ch);
        used += ch_width;
    }
    output
}

pub fn display_width(text: &str) -> usize {
    text.chars().map(char_width).sum()
}

pub fn sidebar_row_prefix(style: SidebarRowStyle) -> &'static str {
    match style {
        SidebarRowStyle::Normal => ANSI_BG_SIDEBAR_ITEM,
        SidebarRowStyle::Current => ANSI_BG_SIDEBAR_ITEM,
        SidebarRowStyle::Selected => ANSI_BG_SIDEBAR_ACTIVE,
    }
}

fn pad_right(text: &str, width: usize) -> String {
    let text = truncate_display_width(text, width);
    let padding = width.saturating_sub(display_width(&text));
    format!("{text}{}", " ".repeat(padding))
}

fn char_width(ch: char) -> usize {
    if ch.is_ascii() || is_single_width_non_ascii(ch) {
        1
    } else {
        2
    }
}

fn is_single_width_non_ascii(ch: char) -> bool {
    matches!(ch, '\u{2500}'..='\u{257F}')
}

#[cfg(test)]
mod tests {
    use super::{
        display_width, right_align, style_sidebar_badge, SidebarBadgeState, SidebarRowStyle,
    };

    #[test]
    fn right_align_truncates_and_right_aligns_display_width() {
        assert_eq!(
            right_align("bash@local | INPUT", 24),
            "      bash@local | INPUT"
        );
    }

    #[test]
    fn running_badge_reports_double_width_emoji_space() {
        let (_, width) =
            style_sidebar_badge(SidebarBadgeState::Running, SidebarRowStyle::Selected, 0);

        assert_eq!(width, display_width("🍳R"));
    }

    #[test]
    fn box_drawing_characters_are_treated_as_single_width() {
        assert_eq!(display_width("────"), 4);
    }
}
