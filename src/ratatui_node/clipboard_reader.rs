//! Cross-platform clipboard reader with fallbacks for native, WSL, X11 and Wayland.

use crate::infra::error_log::ERROR_LOG;
use crate::ratatui_node::clipboard_platform::{is_wsl, windows_path_to_wsl};
use serde::Deserialize;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

/// Clipboard backend that returns decoded plain text.
type TextBackend = Box<dyn Fn() -> Result<String, String>>;
/// Clipboard backend that returns PNG image bytes and a filename hint.
type ImageBackend = Box<dyn Fn() -> Result<(Vec<u8>, String), String>>;

/// Content read from the system clipboard.
#[derive(Debug)]
pub enum ClipboardContent {
    PlainText(String),
    /// Raw `file://` URI strings as returned by the clipboard.
    ///
    /// Platform-specific path resolution is performed by the caller using
    /// [`crate::ratatui_node::clipboard_platform::PlatformContext`].
    FileUris(Vec<String>),
    BinaryFile {
        filename_hint: String,
        bytes: Vec<u8>,
    },
}

const WSL_CLIPBOARD_JSON_COMMAND: &str = "[Console]::OutputEncoding = [System.Text.Encoding]::UTF8; \
     @{ text = Get-Clipboard; files = @((Get-Clipboard -Format FileDropList) | ForEach-Object { $_.FullName }) } | \
     ConvertTo-Json -Compress";

#[derive(Debug, Deserialize)]
struct WslClipboardJson {
    text: Option<String>,
    files: Vec<String>,
}

/// Read the clipboard and classify its contents.
pub fn read_clipboard() -> Result<ClipboardContent, String> {
    // In WSL use a single PowerShell invocation that fetches both text and the
    // Windows file-drop list. Starting powershell.exe from WSL is slow; doing
    // it twice (text then files) caused noticeable lag on Shift+P.
    if is_wsl() {
        match read_clipboard_wsl() {
            Ok(Some(content)) => {
                ERROR_LOG.log(format!(
                    "[clipboard-reader] wsl combined read returned {:?}",
                    std::mem::discriminant(&content)
                ));
                return Ok(content);
            }
            Ok(None) => {
                ERROR_LOG.log("[clipboard-reader] wsl combined read returned empty".to_string());
            }
            Err(error) => {
                ERROR_LOG.log(format!(
                    "[clipboard-reader] wsl combined read failed, falling back: {error}"
                ));
            }
        }
    }

    // Try text/URI-list first (fast, works in most environments including WSL).
    match read_text() {
        Ok(text) if !text.trim().is_empty() => {
            let content = classify_text(&text);
            ERROR_LOG.log(format!(
                "[clipboard-reader] classified text len={} as {:?}",
                text.len(),
                std::mem::discriminant(&content)
            ));
            return Ok(content);
        }
        Ok(_) => {
            ERROR_LOG.log("[clipboard-reader] text backends returned empty".to_string());
        }
        Err(error) => {
            ERROR_LOG.log(format!("[clipboard-reader] text backends failed: {error}"));
        }
    }

    // Text backends could not access the clipboard at all (e.g. no X11 on
    // WSL). Fall back to image as a last resort; the caller decides whether
    // to keep or ignore binary content.
    read_image().map(|(bytes, hint)| ClipboardContent::BinaryFile {
        filename_hint: hint,
        bytes,
    })
}

/// WSL-only combined clipboard read: text + file-drop list in one powershell.exe call.
fn read_clipboard_wsl() -> Result<Option<ClipboardContent>, String> {
    let bytes = run_command_bytes(
        "powershell.exe",
        &["-NoProfile", "-Command", WSL_CLIPBOARD_JSON_COMMAND],
    )?;
    let json = decode_powershell_output(&bytes)?;
    let parsed: WslClipboardJson =
        serde_json::from_str(&json).map_err(|e| format!("invalid WSL clipboard JSON: {e}"))?;

    if let Some(text) = parsed.text {
        if !text.trim().is_empty() {
            return Ok(Some(classify_text(&text)));
        }
    }

    if !parsed.files.is_empty() {
        let uris: Vec<String> = parsed
            .files
            .iter()
            .map(|path| windows_path_to_file_uri(path))
            .collect();
        return Ok(Some(ClipboardContent::FileUris(uris)));
    }

    Ok(None)
}

/// Read plain text or URI list from the clipboard using the first working backend.
fn read_text() -> Result<String, String> {
    // In WSL the Linux clipboard backends (arboard/xclip/wl-paste) have no
    // display to talk to and may hang while probing. Use the Windows bridge
    // first, then fall back to the native Linux backends.
    let backends: Vec<(&str, TextBackend)> = if is_wsl() {
        vec![
            ("powershell (wsl)", Box::new(read_text_powershell)),
            ("arboard", Box::new(read_text_arboard)),
            ("wl-paste", Box::new(read_text_wlpaste)),
            ("xclip", Box::new(read_text_xclip)),
        ]
    } else {
        vec![
            ("arboard", Box::new(read_text_arboard)),
            ("wl-paste", Box::new(read_text_wlpaste)),
            ("xclip", Box::new(read_text_xclip)),
        ]
    };

    let mut errors = Vec::new();
    for (name, backend) in backends {
        match backend() {
            Ok(text) => {
                ERROR_LOG.log(format!("[clipboard-reader] text backend {name} ok"));
                return Ok(text);
            }
            Err(error) => {
                ERROR_LOG.log(format!(
                    "[clipboard-reader] text backend {name} failed: {error}"
                ));
                errors.push(format!("{name}: {error}"));
            }
        }
    }
    Err(format!(
        "all clipboard text backends failed: {}",
        errors.join("; ")
    ))
}

/// Read a list of file paths from the Windows clipboard file-drop format.
///
/// Only useful in WSL: the Windows side of the clipboard exposes copied files
/// as `FileDropList`, which yields raw Windows paths like `C:\foo.txt`.
#[allow(dead_code)]
fn read_file_drop_list() -> Result<Vec<String>, String> {
    let text = read_file_drop_list_powershell()?;
    let paths: Vec<String> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(windows_path_to_file_uri)
        .collect();
    if paths.is_empty() {
        return Err("file drop list is empty".to_string());
    }
    Ok(paths)
}

#[allow(dead_code)]
fn read_file_drop_list_powershell() -> Result<String, String> {
    let bytes = run_command_bytes(
        "powershell.exe",
        &[
            "-NoProfile",
            "-Command",
            "[Console]::OutputEncoding = [System.Text.Encoding]::UTF8; (Get-Clipboard -Format FileDropList) -join \"`n\"",
        ],
    )?;
    decode_powershell_output(&bytes)
}

fn windows_path_to_file_uri(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    format!("file:///{normalized}")
}

/// Read image bytes from the clipboard using the first working backend.
fn read_image() -> Result<(Vec<u8>, String), String> {
    // In WSL prefer the Windows bridge to avoid hanging on X11/Wayland probes.
    let backends: Vec<(&str, ImageBackend)> = if is_wsl() {
        vec![
            ("powershell image (wsl)", Box::new(read_image_powershell)),
            ("arboard", Box::new(read_image_arboard)),
            ("wl-paste image/png", Box::new(read_image_wlpaste)),
            ("xclip image/png", Box::new(read_image_xclip)),
        ]
    } else {
        vec![
            ("arboard", Box::new(read_image_arboard)),
            ("wl-paste image/png", Box::new(read_image_wlpaste)),
            ("xclip image/png", Box::new(read_image_xclip)),
        ]
    };

    let mut errors = Vec::new();
    for (name, backend) in backends {
        match backend() {
            Ok(result) => {
                ERROR_LOG.log(format!("[clipboard-reader] image backend {name} ok"));
                return Ok(result);
            }
            Err(error) => {
                ERROR_LOG.log(format!(
                    "[clipboard-reader] image backend {name} failed: {error}"
                ));
                errors.push(format!("{name}: {error}"));
            }
        }
    }
    Err(format!(
        "all clipboard image backends failed: {}",
        errors.join("; ")
    ))
}

fn read_text_arboard() -> Result<String, String> {
    let mut clipboard = arboard::Clipboard::new().map_err(|e| e.to_string())?;
    clipboard.get_text().map_err(|e| e.to_string())
}

fn read_text_wlpaste() -> Result<String, String> {
    run_command("wl-paste", &["--type", "text/plain"])
}

fn read_text_xclip() -> Result<String, String> {
    run_command("xclip", &["-selection", "clipboard", "-o"])
}

fn read_text_powershell() -> Result<String, String> {
    // Force UTF-8 console output encoding before reading. On non-English
    // Windows systems the default console code page (e.g. GBK/936) causes
    // non-ASCII clipboard text to be returned as invalid UTF-8 bytes.
    let bytes = run_command_bytes(
        "powershell.exe",
        &[
            "-NoProfile",
            "-Command",
            "[Console]::OutputEncoding = [System.Text.Encoding]::UTF8; Get-Clipboard",
        ],
    )?;
    decode_powershell_output(&bytes)
}

fn decode_utf16le(bytes: &[u8]) -> Result<String, String> {
    if bytes.len() % 2 != 0 {
        return Err("UTF-16-LE data has odd byte length".to_string());
    }
    let u16s: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
        .collect();
    String::from_utf16(&u16s).map_err(|e| format!("invalid UTF-16-LE: {e}"))
}

fn decode_utf16be(bytes: &[u8]) -> Result<String, String> {
    if bytes.len() % 2 != 0 {
        return Err("UTF-16-BE data has odd byte length".to_string());
    }
    let u16s: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|chunk| u16::from_be_bytes([chunk[0], chunk[1]]))
        .collect();
    String::from_utf16(&u16s).map_err(|e| format!("invalid UTF-16-BE: {e}"))
}

/// Decode PowerShell stdout bytes to a String, handling UTF-16 BOM and UTF-8.
fn decode_powershell_output(bytes: &[u8]) -> Result<String, String> {
    if bytes.starts_with(&[0xff, 0xfe]) {
        return decode_utf16le(&bytes[2..]);
    }
    if bytes.starts_with(&[0xfe, 0xff]) {
        return decode_utf16be(&bytes[2..]);
    }
    String::from_utf8(bytes.to_vec())
        .map_err(|e| format!("powershell output is not utf-8: {e}"))
        .map(|s| s.trim_end_matches('\r').trim_end_matches('\n').to_string())
}

fn read_image_arboard() -> Result<(Vec<u8>, String), String> {
    let mut clipboard = arboard::Clipboard::new().map_err(|e| e.to_string())?;
    let image = clipboard.get_image().map_err(|e| e.to_string())?;
    let bytes = encode_image_to_png(&image)?;
    Ok((bytes, "clipboard.png".to_string()))
}

fn read_image_wlpaste() -> Result<(Vec<u8>, String), String> {
    let bytes = run_command_bytes("wl-paste", &["--type", "image/png"])?;
    Ok((bytes, "clipboard.png".to_string()))
}

fn read_image_xclip() -> Result<(Vec<u8>, String), String> {
    let bytes = run_command_bytes(
        "xclip",
        &["-selection", "clipboard", "-t", "image/png", "-o"],
    )?;
    Ok((bytes, "clipboard.png".to_string()))
}

/// WSL fallback: ask Windows PowerShell to save the clipboard image to a shared
/// temp file, then read it from WSL.
fn read_image_powershell() -> Result<(Vec<u8>, String), String> {
    let windows_temp = std::env::var("TEMP")
        .or_else(|_| std::env::var("TMP"))
        .unwrap_or_else(|_| "C:\\Windows\\Temp".to_string());
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis().to_string())
        .unwrap_or_else(|_| "0".to_string());
    let windows_file = format!("{}\\waitagent-paste-{}.png", windows_temp, timestamp);
    let wsl_path = windows_path_to_wsl(&windows_file);

    let script = format!(
        "Add-Type -AssemblyName System.Windows.Forms; \
         $img = [System.Windows.Forms.Clipboard]::GetImage(); \
         if ($img -eq $null) {{ throw 'no image on clipboard' }}; \
         $img.Save('{}')",
        windows_file.replace('\\', "\\\\").replace('\'', "''")
    );
    run_command("powershell.exe", &["-NoProfile", "-Command", &script])?;

    let bytes = std::fs::read(&wsl_path)
        .map_err(|e| format!("failed to read WSL temp image {}: {e}", wsl_path.display()))?;
    let _ = std::fs::remove_file(&wsl_path);
    Ok((bytes, "clipboard.png".to_string()))
}

fn run_command(cmd: &str, args: &[&str]) -> Result<String, String> {
    let output = Command::new(cmd)
        .args(args)
        .output()
        .map_err(|e| format!("{cmd} failed: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("{cmd} exited {:?}: {stderr}", output.status));
    }
    String::from_utf8(output.stdout).map_err(|e| format!("{cmd} output is not utf-8: {e}"))
}

fn run_command_bytes(cmd: &str, args: &[&str]) -> Result<Vec<u8>, String> {
    let output = Command::new(cmd)
        .args(args)
        .output()
        .map_err(|e| format!("{cmd} failed: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("{cmd} exited {:?}: {stderr}", output.status));
    }
    Ok(output.stdout)
}

/// Classify clipboard text as plain text or a list of `file://` URIs.
///
/// URIs are returned as raw strings; platform-specific path resolution is the
/// caller's responsibility.
fn classify_text(text: &str) -> ClipboardContent {
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

fn encode_image_to_png(image: &arboard::ImageData) -> Result<Vec<u8>, String> {
    let pixel_count = image.width.saturating_mul(image.height);
    if pixel_count == 0 {
        return Err("clipboard image is empty".to_string());
    }
    let expected_bytes = pixel_count.saturating_mul(4);
    if image.bytes.len() != expected_bytes {
        return Err(format!(
            "clipboard image size mismatch: {}x{} expects {} bytes, got {}",
            image.width,
            image.height,
            expected_bytes,
            image.bytes.len()
        ));
    }
    const MAX_IMAGE_PIXELS: usize = 20_000_000;
    if pixel_count > MAX_IMAGE_PIXELS {
        return Err(format!(
            "clipboard image too large: {}x{} exceeds {} pixels",
            image.width, image.height, MAX_IMAGE_PIXELS
        ));
    }

    let rgba = image.bytes.to_vec();
    let rgba_image = image::RgbaImage::from_raw(image.width as u32, image.height as u32, rgba)
        .ok_or_else(|| "clipboard image dimensions do not match byte length".to_string())?;
    let dynamic = image::DynamicImage::ImageRgba8(rgba_image);
    let mut buffer = std::io::Cursor::new(Vec::new());
    dynamic
        .write_to(&mut buffer, image::ImageFormat::Png)
        .map_err(|e| format!("failed to encode clipboard image: {e}"))?;
    Ok(buffer.into_inner())
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

    #[test]
    fn decode_utf16le_handles_bom() {
        // "test" encoded as UTF-16-LE with BOM.
        let bytes: Vec<u8> = vec![0x74, 0x00, 0x65, 0x00, 0x73, 0x00, 0x74, 0x00];
        assert_eq!(decode_utf16le(&bytes).unwrap(), "test");
    }

    #[test]
    fn decode_utf16le_handles_chinese() {
        // "板卡" encoded as UTF-16-LE.
        // 板 U+677F -> 0x7F 0x67; 卡 U+5361 -> 0x61 0x53
        let bytes: Vec<u8> = vec![0x7f, 0x67, 0x61, 0x53];
        assert_eq!(decode_utf16le(&bytes).unwrap(), "板卡");
    }
}
