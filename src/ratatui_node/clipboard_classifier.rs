//! Classify clipboard text into plain text or a list of `file://` URIs.
//!
//! This module does not touch the system clipboard; it only inspects strings
//! returned by the clipboard reader and decides how they should be handled by
//! the paste dispatcher.

/// Content read from the system clipboard, after classification.
#[derive(Debug)]
pub enum ClipboardContent {
    PlainText(String),
    /// Raw `file://` URI strings as returned by the clipboard.
    ///
    /// Platform-specific path resolution is performed by the paste dispatcher
    /// using [`crate::ratatui_node::clipboard_platform::PlatformContext`].
    FileUris(Vec<String>),
    BinaryFile {
        filename_hint: String,
        bytes: Vec<u8>,
    },
}

/// Classify clipboard text as plain text or a list of `file://` URIs.
///
/// URIs are returned as raw strings; platform-specific path resolution is the
/// caller's responsibility.
pub fn classify_text(text: &str) -> ClipboardContent {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    if lines.is_empty() {
        return ClipboardContent::PlainText(text.to_string());
    }

    let mut uris = Vec::with_capacity(lines.len());
    for line in &lines {
        if is_file_uri(line) {
            uris.push(line.to_string());
        } else {
            return ClipboardContent::PlainText(text.to_string());
        }
    }

    ClipboardContent::FileUris(uris)
}

/// Return true if `line` looks like a `file://` URI.
fn is_file_uri(line: &str) -> bool {
    line.starts_with("file://")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_plain_text() {
        match classify_text("hello") {
            ClipboardContent::PlainText(t) => assert_eq!(t, "hello"),
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[test]
    fn classify_file_uri() {
        match classify_text("file:///home/user/my file.txt") {
            ClipboardContent::FileUris(uris) => {
                assert_eq!(uris.len(), 1);
                assert_eq!(uris[0], "file:///home/user/my file.txt");
            }
            other => panic!("expected URIs, got {other:?}"),
        }
    }

    #[test]
    fn classify_uri_list() {
        let input = "file:///home/user/a.txt\r\nfile:///home/user/b.txt\r\n";
        match classify_text(input) {
            ClipboardContent::FileUris(uris) => {
                assert_eq!(uris.len(), 2);
                assert_eq!(uris[0], "file:///home/user/a.txt");
                assert_eq!(uris[1], "file:///home/user/b.txt");
            }
            other => panic!("expected URIs, got {other:?}"),
        }
    }

    #[test]
    fn classify_mixed_lines_falls_back_to_text() {
        // If one line is not a file URI, treat the whole clipboard as text.
        match classify_text("file:///home/user/a.txt\nnot a uri") {
            ClipboardContent::PlainText(text) => {
                assert_eq!(text, "file:///home/user/a.txt\nnot a uri");
            }
            other => panic!("expected plain text fallback, got {other:?}"),
        }
    }
}
