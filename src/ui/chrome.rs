const ANSI_RESET: &str = "\x1b[0m";
const ANSI_BG_BAR: &str = "\x1b[48;5;24m\x1b[38;5;255m";
const ANSI_FOOTER_KEY: &str = "\x1b[48;5;24m\x1b[1;38;5;159m";
const ANSI_FOOTER_MUTED: &str = "\x1b[48;5;24m\x1b[38;5;110m";
const ANSI_FOOTER_NETWORK: &str = "\x1b[48;5;24m\x1b[1;38;5;121m";

pub fn style_status_line(line: &str, width: usize) -> String {
    let line = pad_right(line, width);
    format!("{ANSI_BG_BAR}{}{ANSI_RESET}", style_footer_words(&line))
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

fn pad_right(text: &str, width: usize) -> String {
    let text = truncate_display_width(text, width);
    let padding = width.saturating_sub(display_width(&text));
    format!("{text}{}", " ".repeat(padding))
}

fn style_footer_words(line: &str) -> String {
    let mut output = String::new();
    let mut word = String::new();

    for ch in line.chars() {
        if ch.is_whitespace() {
            push_styled_footer_word(&mut output, &word);
            word.clear();
            output.push(ch);
        } else {
            word.push(ch);
        }
    }
    push_styled_footer_word(&mut output, &word);
    output
}

fn push_styled_footer_word(output: &mut String, word: &str) {
    if word.is_empty() {
        return;
    }
    let style = if is_footer_key(word) {
        ANSI_FOOTER_KEY
    } else if is_footer_network_label(word) {
        ANSI_FOOTER_NETWORK
    } else if is_footer_muted(word) {
        ANSI_FOOTER_MUTED
    } else {
        ANSI_BG_BAR
    };
    output.push_str(style);
    output.push_str(word);
    output.push_str(ANSI_BG_BAR);
}

fn is_footer_key(word: &str) -> bool {
    matches!(
        word,
        "Ctrl-N"
            | "Ctrl-W"
            | "Ctrl-S"
            | "Ctrl-O"
            | "Ctrl-E"
            | "Ctrl-M"
            | "PgUp/PgDn"
            | "Up/Down"
            | "q"
    )
}

fn is_footer_network_label(word: &str) -> bool {
    matches!(word, "Listen" | "Connect" | "View")
}

fn is_footer_muted(word: &str) -> bool {
    matches!(word, "│" | "·")
}

fn char_width(ch: char) -> usize {
    if matches!(ch, '\u{fe0e}' | '\u{fe0f}') {
        0
    } else if ch.is_ascii() || is_single_width_non_ascii(ch) {
        1
    } else {
        2
    }
}

fn is_single_width_non_ascii(ch: char) -> bool {
    matches!(ch, '\u{2500}'..='\u{257F}' | '·' | '…')
}

#[cfg(test)]
mod tests {
    use super::display_width;

    #[test]
    fn box_drawing_characters_are_treated_as_single_width() {
        assert_eq!(display_width("────"), 4);
    }

    #[test]
    fn footer_separators_are_treated_as_single_width() {
        assert_eq!(display_width("·…"), 2);
    }
}
