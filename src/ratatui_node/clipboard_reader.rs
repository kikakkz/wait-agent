//! Cross-platform clipboard reader with fallbacks for native, WSL, X11 and Wayland.
//!
//! This module is only responsible for reading raw content from the system
//! clipboard. Classification (text vs URI list) is performed by
//! [`crate::ratatui_node::clipboard_classifier`].

use crate::infra::error_log::ERROR_LOG;
use crate::ratatui_node::clipboard_platform::is_wsl;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

/// Clipboard backend that returns decoded plain text.
type TextBackend = Box<dyn Fn() -> Result<String, String>>;
/// Clipboard backend that returns PNG image bytes and a filename hint.
type ImageBackend = Box<dyn Fn() -> Result<(Vec<u8>, String), String>>;

/// Raw content read from the system clipboard.
#[derive(Debug)]
pub enum ClipboardReadResult {
    /// Plain text or a newline-separated list of `file://` URIs.
    Text(String),
    /// Binary image data read from the clipboard.
    Image {
        filename_hint: String,
        bytes: Vec<u8>,
    },
    /// The clipboard is empty or contains no readable text/image.
    Empty,
}

const WSL_CLIPBOARD_JSON_COMMAND: &str = "[Console]::OutputEncoding = [System.Text.Encoding]::UTF8; \
     @{ text = Get-Clipboard; files = @((Get-Clipboard -Format FileDropList) | ForEach-Object { $_.FullName }) } | \
     ConvertTo-Json -Compress";

#[derive(Debug, serde::Deserialize)]
struct WslClipboardJson {
    text: Option<String>,
    files: Vec<String>,
}

/// Read the system clipboard and return its raw content.
///
/// Text/URI-list is tried first. If the text backends report an empty
/// clipboard, image backends are attempted so that copied screenshots can
/// still be pasted via explicit keyboard shortcuts.
pub fn read_clipboard() -> Result<ClipboardReadResult, String> {
    let start = std::time::Instant::now();

    // In WSL use a single PowerShell invocation that fetches both text and the
    // Windows file-drop list. Starting powershell.exe from WSL is slow; doing
    // it twice (text then files) caused noticeable lag.
    if is_wsl() {
        match read_clipboard_wsl() {
            Ok(ClipboardReadResult::Empty) => {
                ERROR_LOG.log("[clipboard-reader] wsl combined read returned empty".to_string());
            }
            Ok(result) => {
                ERROR_LOG.log(format!(
                    "[clipboard-reader] wsl combined read returned {:?} in {:?}",
                    std::mem::discriminant(&result),
                    start.elapsed()
                ));
                return Ok(result);
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
            ERROR_LOG.log(format!(
                "[clipboard-reader] text backend ok len={} in {:?}",
                text.len(),
                start.elapsed()
            ));
            return Ok(ClipboardReadResult::Text(text));
        }
        Ok(_) => {
            ERROR_LOG.log("[clipboard-reader] text backends returned empty".to_string());
        }
        Err(error) => {
            ERROR_LOG.log(format!("[clipboard-reader] text backends failed: {error}"));
        }
    }

    // Text backends could not access the clipboard at all (e.g. no X11 on
    // WSL) or the clipboard is empty. Try image as a last resort.
    match read_image() {
        Ok((bytes, hint)) => {
            ERROR_LOG.log(format!(
                "[clipboard-reader] image backend ok bytes={} in {:?}",
                bytes.len(),
                start.elapsed()
            ));
            Ok(ClipboardReadResult::Image {
                filename_hint: hint,
                bytes,
            })
        }
        Err(error) => {
            ERROR_LOG.log(format!(
                "[clipboard-reader] all backends failed in {:?}: {error}",
                start.elapsed()
            ));
            Err(error)
        }
    }
}

/// Strip a single trailing line ending from text fetched from the clipboard.
///
/// Windows applications frequently store a trailing `\r\n` when copying text.
/// When the TUI injects clipboard bytes as keyboard input, that trailing newline
/// would execute the command or add an unwanted blank line. Removing one
/// trailing newline normalizes the clipboard content without destroying
/// intentional multi-line text.
pub fn normalize_clipboard_text(text: &str) -> &str {
    text.strip_suffix("\r\n")
        .or_else(|| text.strip_suffix('\n'))
        .or_else(|| text.strip_suffix('\r'))
        .unwrap_or(text)
}

/// WSL-only combined clipboard read: text + file-drop list in one powershell.exe call.
fn read_clipboard_wsl() -> Result<ClipboardReadResult, String> {
    let start = std::time::Instant::now();
    let bytes = run_command_bytes(
        "powershell.exe",
        &["-NoProfile", "-Command", WSL_CLIPBOARD_JSON_COMMAND],
    )?;
    let json = decode_powershell_output(&bytes)?;
    let parsed: WslClipboardJson =
        serde_json::from_str(&json).map_err(|e| format!("invalid WSL clipboard JSON: {e}"))?;

    if let Some(text) = parsed.text {
        let text = normalize_clipboard_text(&text);
        if !text.trim().is_empty() {
            return Ok(ClipboardReadResult::Text(text.to_string()));
        }
    }

    if !parsed.files.is_empty() {
        let uris: Vec<String> = parsed
            .files
            .iter()
            .map(|path| windows_path_to_file_uri(path))
            .collect();
        return Ok(ClipboardReadResult::Text(uris.join("\n")));
    }

    ERROR_LOG.log(format!(
        "[clipboard-reader] wsl combined read empty in {:?}",
        start.elapsed()
    ));
    Ok(ClipboardReadResult::Empty)
}

/// Read plain text or URI list from the clipboard using the first working backend.
fn read_text() -> Result<String, String> {
    // Select backends based on the runtime platform. Avoid cross-platform
    // probes (e.g. PowerShell on native Linux) that can only fail.
    let backends: Vec<(&str, TextBackend)> = if is_wsl() {
        vec![
            ("powershell (wsl)", Box::new(read_text_powershell)),
            ("arboard", Box::new(read_text_arboard)),
        ]
    } else if cfg!(target_os = "linux") {
        vec![
            ("arboard", Box::new(read_text_arboard)),
            ("wl-paste", Box::new(read_text_wlpaste)),
            ("xclip", Box::new(read_text_xclip)),
        ]
    } else {
        vec![("arboard", Box::new(read_text_arboard))]
    };

    let mut errors = Vec::new();
    for (name, backend) in backends {
        let start = std::time::Instant::now();
        match backend() {
            Ok(text) => {
                ERROR_LOG.log(format!(
                    "[clipboard-reader] text backend {name} ok in {:?}",
                    start.elapsed()
                ));
                return Ok(normalize_clipboard_text(&text).to_string());
            }
            Err(error) => {
                ERROR_LOG.log(format!(
                    "[clipboard-reader] text backend {name} failed in {:?}: {error}",
                    start.elapsed()
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

/// Read image bytes from the clipboard using the first working backend.
fn read_image() -> Result<(Vec<u8>, String), String> {
    // In WSL prefer the Windows bridge to avoid hanging on X11/Wayland probes.
    let backends: Vec<(&str, ImageBackend)> = if is_wsl() {
        vec![
            ("powershell image (wsl)", Box::new(read_image_powershell)),
            ("arboard", Box::new(read_image_arboard)),
        ]
    } else if cfg!(target_os = "linux") {
        vec![
            ("arboard", Box::new(read_image_arboard)),
            ("wl-paste image/png", Box::new(read_image_wlpaste)),
            ("xclip image/png", Box::new(read_image_xclip)),
        ]
    } else {
        vec![("arboard", Box::new(read_image_arboard))]
    };

    let mut errors = Vec::new();
    for (name, backend) in backends {
        let start = std::time::Instant::now();
        match backend() {
            Ok(result) => {
                ERROR_LOG.log(format!(
                    "[clipboard-reader] image backend {name} ok in {:?}",
                    start.elapsed()
                ));
                return Ok(result);
            }
            Err(error) => {
                ERROR_LOG.log(format!(
                    "[clipboard-reader] image backend {name} failed in {:?}: {error}",
                    start.elapsed()
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
    let wsl_path = crate::ratatui_node::clipboard_platform::windows_path_to_wsl(&windows_file);

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
    let start = std::time::Instant::now();
    let output = Command::new(cmd)
        .args(args)
        .output()
        .map_err(|e| format!("{cmd} failed: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("{cmd} exited {:?}: {stderr}", output.status));
    }
    ERROR_LOG.log(format!(
        "[clipboard-reader] ran {cmd} in {:?}",
        start.elapsed()
    ));
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

fn windows_path_to_file_uri(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    format!("file:///{normalized}")
}
