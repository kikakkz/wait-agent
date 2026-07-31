use super::logical_key::{KeyCode, KeyModifiers, LogicalKey};

/// Terminal-mode flags that affect how logical keys are translated into bytes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct KeyTranslationMode {
    /// When true, cursor keys (arrows, Home, End) are sent as SS3 sequences
    /// (`\x1bOA`) instead of CSI sequences (`\x1b[A`).
    pub application_cursor_keys: bool,
    /// Reserved for keypad application-mode translation.
    pub application_keypad: bool,
}

/// Translate a logical key into the bytes that should be written to a PTY.
///
/// The translation depends on the terminal's current mode, in particular
/// `application_cursor_keys`. Unmapped keys produce an empty vector.
pub fn translate_key(key: &LogicalKey, mode: KeyTranslationMode) -> Vec<u8> {
    match &key.code {
        KeyCode::Char(c) => translate_char(*c, key.modifiers),
        KeyCode::F(n) => translate_f(*n),
        KeyCode::Up => cursor_or_ss3(b'A', mode.application_cursor_keys),
        KeyCode::Down => cursor_or_ss3(b'B', mode.application_cursor_keys),
        KeyCode::Right => cursor_or_ss3(b'C', mode.application_cursor_keys),
        KeyCode::Left => cursor_or_ss3(b'D', mode.application_cursor_keys),
        KeyCode::Home => cursor_or_ss3(b'H', mode.application_cursor_keys),
        KeyCode::End => cursor_or_ss3(b'F', mode.application_cursor_keys),
        KeyCode::PageUp => csi(b'~', Some(b'5')),
        KeyCode::PageDown => csi(b'~', Some(b'6')),
        KeyCode::Insert => csi(b'~', Some(b'2')),
        KeyCode::Delete => csi(b'~', Some(b'3')),
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Tab => {
            if key.modifiers.shift {
                vec![0x1b, b'[', b'Z']
            } else {
                vec![b'\t']
            }
        }
        KeyCode::BackTab => vec![0x1b, b'[', b'Z'],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Null => vec![0x00],
        KeyCode::Unsupported(_) => Vec::new(),
    }
}

fn translate_char(c: char, modifiers: KeyModifiers) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(8);

    if modifiers.alt {
        bytes.push(0x1b);
    }

    if modifiers.control {
        if let Some(ctrl) = char_to_control_byte(c) {
            bytes.push(ctrl);
            return bytes;
        }
    }

    let mut buf = [0u8; 4];
    bytes.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
    bytes
}

fn char_to_control_byte(c: char) -> Option<u8> {
    match c {
        'a'..='z' => Some(c as u8 - b'a' + 1),
        'A'..='Z' => Some(c as u8 - b'A' + 1),
        ' ' => Some(0),
        '\\' => Some(0x1c),
        '[' => Some(0x1b),
        ']' => Some(0x1d),
        '^' => Some(0x1e),
        '_' => Some(0x1f),
        '?' => Some(0x7f),
        '@' => Some(0),
        _ => None,
    }
}

fn translate_f(n: u8) -> Vec<u8> {
    match n {
        1 => vec![0x1b, b'O', b'P'],
        2 => vec![0x1b, b'O', b'Q'],
        3 => vec![0x1b, b'O', b'R'],
        4 => vec![0x1b, b'O', b'S'],
        5..=12 => {
            let mut v = vec![0x1b, b'['];
            v.extend_from_slice(f_sequence_prefix(n));
            v.push(b'~');
            v
        }
        _ => Vec::new(),
    }
}

fn f_sequence_prefix(n: u8) -> &'static [u8] {
    match n {
        5 => b"15",
        6 => b"17",
        7 => b"18",
        8 => b"19",
        9 => b"20",
        10 => b"21",
        11 => b"23",
        12 => b"24",
        _ => b"",
    }
}

fn cursor_or_ss3(final_byte: u8, application: bool) -> Vec<u8> {
    if application {
        vec![0x1b, b'O', final_byte]
    } else {
        vec![0x1b, b'[', final_byte]
    }
}

fn csi(final_byte: u8, prefix: Option<u8>) -> Vec<u8> {
    let mut v = vec![0x1b, b'['];
    if let Some(p) = prefix {
        v.push(p);
    }
    v.push(final_byte);
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> LogicalKey {
        LogicalKey {
            code,
            modifiers: KeyModifiers::default(),
        }
    }

    fn ctrl(c: char) -> LogicalKey {
        LogicalKey {
            code: KeyCode::Char(c),
            modifiers: KeyModifiers {
                control: true,
                ..Default::default()
            },
        }
    }

    fn alt(c: char) -> LogicalKey {
        LogicalKey {
            code: KeyCode::Char(c),
            modifiers: KeyModifiers {
                alt: true,
                ..Default::default()
            },
        }
    }

    fn normal() -> KeyTranslationMode {
        KeyTranslationMode::default()
    }

    fn app_cursor() -> KeyTranslationMode {
        KeyTranslationMode {
            application_cursor_keys: true,
            application_keypad: false,
        }
    }

    #[test]
    fn char_passes_through() {
        assert_eq!(translate_key(&key(KeyCode::Char('a')), normal()), b"a");
        assert_eq!(translate_key(&key(KeyCode::Char('A')), normal()), b"A");
    }

    #[test]
    fn control_letters_become_control_bytes() {
        assert_eq!(translate_key(&ctrl('c'), normal()), b"\x03");
        assert_eq!(translate_key(&ctrl('r'), normal()), b"\x12");
        assert_eq!(translate_key(&ctrl('C'), normal()), b"\x03");
    }

    #[test]
    fn alt_prefixes_escape() {
        assert_eq!(translate_key(&alt('b'), normal()), b"\x1bb");
    }

    #[test]
    fn cursor_keys_follow_mode() {
        assert_eq!(translate_key(&key(KeyCode::Up), normal()), b"\x1b[A");
        assert_eq!(translate_key(&key(KeyCode::Up), app_cursor()), b"\x1bOA");
        assert_eq!(translate_key(&key(KeyCode::Down), normal()), b"\x1b[B");
        assert_eq!(translate_key(&key(KeyCode::Down), app_cursor()), b"\x1bOB");
        assert_eq!(translate_key(&key(KeyCode::Right), normal()), b"\x1b[C");
        assert_eq!(translate_key(&key(KeyCode::Left), normal()), b"\x1b[D");
        assert_eq!(translate_key(&key(KeyCode::Home), app_cursor()), b"\x1bOH");
        assert_eq!(translate_key(&key(KeyCode::End), app_cursor()), b"\x1bOF");
    }

    #[test]
    fn fixed_keys() {
        assert_eq!(translate_key(&key(KeyCode::Enter), normal()), b"\r");
        assert_eq!(translate_key(&key(KeyCode::Backspace), normal()), b"\x7f");
        assert_eq!(translate_key(&key(KeyCode::Tab), normal()), b"\t");
        assert_eq!(
            translate_key(
                &LogicalKey {
                    code: KeyCode::Tab,
                    modifiers: KeyModifiers {
                        shift: true,
                        ..Default::default()
                    }
                },
                normal()
            ),
            b"\x1b[Z"
        );
        assert_eq!(translate_key(&key(KeyCode::Esc), normal()), b"\x1b");
    }

    #[test]
    fn function_keys() {
        assert_eq!(translate_key(&key(KeyCode::F(1)), normal()), b"\x1bOP");
        assert_eq!(translate_key(&key(KeyCode::F(4)), normal()), b"\x1bOS");
        assert_eq!(translate_key(&key(KeyCode::F(5)), normal()), b"\x1b[15~");
        assert_eq!(translate_key(&key(KeyCode::F(12)), normal()), b"\x1b[24~");
    }

    #[test]
    fn editing_keys() {
        assert_eq!(translate_key(&key(KeyCode::Insert), normal()), b"\x1b[2~");
        assert_eq!(translate_key(&key(KeyCode::Delete), normal()), b"\x1b[3~");
        assert_eq!(translate_key(&key(KeyCode::PageUp), normal()), b"\x1b[5~");
        assert_eq!(translate_key(&key(KeyCode::PageDown), normal()), b"\x1b[6~");
    }

    #[test]
    fn unsupported_keys_are_dropped() {
        assert_eq!(
            translate_key(&key(KeyCode::Unsupported("CapsLock".into())), normal()),
            Vec::<u8>::new()
        );
    }
}
