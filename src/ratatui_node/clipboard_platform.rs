//! Cross-platform path resolution and file I/O for clipboard paste.
//!
//! The clipboard may contain file URIs from different operating systems. The
//! same URI can mean different things depending on where the TUI client is
//! running:
//!
//! - Windows native: `file:///C:/foo.txt` -> `C:\foo.txt`
//! - Linux native: `file:///home/u/foo.txt` -> `/home/u/foo.txt`
//! - WSL: `file:///C:/foo.txt` -> `/mnt/c/foo.txt`
//!   `file:///home/u/foo.txt` -> `/home/u/foo.txt`
//!
//! This module detects the runtime context and resolves URIs accordingly.

use crate::ratatui_node::clipboard_cache::{clipboard_cache_dir, unique_cached_filename};
use std::path::{Path, PathBuf};

/// Runtime execution context of the TUI client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlatformContext {
    /// Native Windows process.
    Windows,
    /// Native Linux process.
    Linux,
    /// Linux process running inside WSL2.
    Wsl,
    /// Native macOS process.
    MacOs,
}

impl PlatformContext {
    /// Detect the current runtime context.
    pub fn detect() -> Self {
        if cfg!(target_os = "windows") {
            return Self::Windows;
        }
        if cfg!(target_os = "macos") {
            return Self::MacOs;
        }
        if std::env::var_os("WAITAGENT_FORCE_WSL").is_some() {
            return Self::Wsl;
        }
        if is_wsl() {
            Self::Wsl
        } else {
            Self::Linux
        }
    }

    /// Convert a `file://` URI into a path that the current process can read.
    ///
    /// Returns `None` if `uri` does not start with `file://`.
    pub fn resolve_file_uri(&self, uri: &str) -> Option<PathBuf> {
        let after_scheme = uri.strip_prefix("file://")?;
        let path_part = after_scheme
            .strip_prefix("localhost")
            .unwrap_or(after_scheme);
        let decoded = percent_decode(path_part);

        match self {
            Self::Windows => Some(windows_uri_path_to_windows(&decoded)),
            Self::Wsl => resolve_wsl_uri_path(&decoded),
            Self::Linux | Self::MacOs => Some(PathBuf::from(decoded)),
        }
    }

    /// Try to interpret `text` as a list of absolute file paths.
    ///
    /// Clipboard text may contain raw paths like `C:\foo.txt` or
    /// `/home/u/foo.txt` instead of `file://` URIs. Returns `Some(paths)` only
    /// if every non-empty line can be resolved to an absolute path for the
    /// current platform.
    pub fn parse_file_paths_from_text(&self, text: &str) -> Option<Vec<PathBuf>> {
        let lines: Vec<&str> = text
            .lines()
            .map(str::trim)
            .map(trim_path_input)
            .filter(|line| !line.is_empty())
            .collect();
        if lines.is_empty() {
            return None;
        }

        let mut paths = Vec::with_capacity(lines.len());
        for line in lines {
            if let Some(path) = self.resolve_file_uri(line) {
                paths.push(path);
            } else if let Some(path) = self.parse_absolute_path(line) {
                paths.push(path);
            } else {
                return None;
            }
        }
        Some(paths)
    }

    /// Parse a raw absolute path string for the current platform.
    fn parse_absolute_path(&self, input: &str) -> Option<PathBuf> {
        match self {
            Self::Windows => parse_windows_absolute_path(input),
            Self::Wsl => parse_wsl_absolute_path(input),
            Self::Linux | Self::MacOs => parse_unix_absolute_path(input),
        }
    }

    /// Read the contents of a file at `path`.
    ///
    /// In WSL this works for both Linux paths (`/home/...`) and Windows paths
    /// mounted under `/mnt/...`.
    pub fn read_file(&self, path: &Path) -> Result<Vec<u8>, String> {
        std::fs::read(path).map_err(|e| format!("failed to read {}: {e}", path.display()))
    }

    /// Write bytes to the platform-appropriate temporary cache directory.
    ///
    /// On Windows this is `%TEMP%\waitagent`. On Linux/WSL/macOS this uses
    /// [`std::env::temp_dir`], which is `/tmp` on Linux/WSL.
    pub fn write_temp_file(&self, filename_hint: &str, bytes: &[u8]) -> Result<PathBuf, String> {
        let _ = self;
        let dir = clipboard_cache_dir();
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("failed to create temp dir {}: {e}", dir.display()))?;

        let filename = unique_cached_filename(filename_hint);
        let path = dir.join(&filename);
        std::fs::write(&path, bytes)
            .map_err(|e| format!("failed to write temp file {}: {e}", path.display()))?;
        Ok(path)
    }

    /// Return a string suitable for sending as keyboard input to a shell.
    pub fn path_for_input(&self, path: &Path) -> String {
        let _ = self;
        path.to_string_lossy().into_owned()
    }
}

/// Format a local file path for injection into the active session.
///
/// Agent sessions that understand `@` references get `@/path/to/file`;
/// plain shells and other agents receive the raw path. Paths containing
/// whitespace are quoted so the reference does not break on spaces.
pub fn format_file_reference(path: &str, supports_at: bool) -> String {
    if !supports_at {
        return path.to_string();
    }
    if path.contains(char::is_whitespace) {
        format!("\"@{path}\"")
    } else {
        format!("@{path}")
    }
}

/// Detect whether the current Linux process is running inside WSL.
pub(crate) fn is_wsl() -> bool {
    if std::env::var("WSL_DISTRO_NAME").is_ok()
        || std::env::var("WSLENV").is_ok()
        || std::env::var("WSL_INTEROP").is_ok()
    {
        return true;
    }
    if Path::new("/proc/sys/fs/binfmt_misc/WSLInterop").exists() {
        return true;
    }
    if let Ok(release) = std::fs::read_to_string("/proc/sys/kernel/osrelease") {
        let lower = release.to_lowercase();
        if lower.contains("microsoft") || lower.contains("wsl") {
            return true;
        }
    }
    if let Ok(version) = std::fs::read_to_string("/proc/version") {
        let lower = version.to_lowercase();
        if lower.contains("microsoft") || lower.contains("wsl") {
            return true;
        }
    }
    false
}

/// Convert a URI path part like `/C:/Users/foo/bar.png` into a Windows path.
fn windows_uri_path_to_windows(path: &str) -> PathBuf {
    // Remove the leading slash that file:// URIs add before the drive letter.
    let trimmed = path.strip_prefix('/').unwrap_or(path);
    PathBuf::from(trimmed.replace('/', "\\"))
}

fn parse_windows_absolute_path(input: &str) -> Option<PathBuf> {
    if looks_like_windows_drive_path(input) || input.starts_with("\\\\") {
        Some(PathBuf::from(input))
    } else {
        None
    }
}

fn parse_wsl_absolute_path(input: &str) -> Option<PathBuf> {
    if input.starts_with('/') {
        Some(PathBuf::from(input))
    } else if looks_like_windows_drive_path(input) {
        Some(windows_path_to_wsl(input))
    } else {
        None
    }
}

fn parse_unix_absolute_path(input: &str) -> Option<PathBuf> {
    if input.starts_with('/') {
        Some(PathBuf::from(input))
    } else {
        None
    }
}

/// Strip surrounding whitespace and matching quotes from a candidate path.
fn trim_path_input(line: &str) -> &str {
    line.trim().trim_matches(|c| c == '"' || c == '\'').trim()
}

/// Return true if `input` looks like a Windows absolute drive path.
fn looks_like_windows_drive_path(input: &str) -> bool {
    input.len() >= 3
        && input.as_bytes()[1] == b':'
        && (input.as_bytes()[2] == b'\\' || input.as_bytes()[2] == b'/')
}

/// Resolve a decoded URI path inside WSL.
///
/// Handles Windows drive letters, WSL network paths, and native Linux paths.
fn resolve_wsl_uri_path(path: &str) -> Option<PathBuf> {
    // file:///C:/Users/foo/bar.png -> /mnt/c/Users/foo/bar.png
    if let Some(rest) = path.strip_prefix('/') {
        if rest.len() >= 2 && rest.as_bytes()[1] == b':' {
            let drive = rest.chars().next()?.to_lowercase().to_string();
            return Some(PathBuf::from(format!(
                "/mnt/{drive}{}",
                &rest[2..].replace('\\', "/")
            )));
        }
    }

    // file:///wsl.localhost/Distro/home/user/foo -> /home/user/foo
    if let Some(rest) = path.strip_prefix("/wsl.localhost/") {
        let parts: Vec<&str> = rest.splitn(2, '/').collect();
        if parts.len() == 2 {
            return Some(PathBuf::from(format!("/{}", parts[1])));
        }
    }

    // file://wsl$/Distro/home/user/foo -> /home/user/foo
    // After stripping `file://` we are left with `wsl$/Distro/...`.
    if let Some(rest) = path.strip_prefix("wsl$/") {
        let parts: Vec<&str> = rest.splitn(2, '/').collect();
        if parts.len() == 2 {
            return Some(PathBuf::from(format!("/{}", parts[1])));
        }
    }

    // Native Linux path or UNC path.
    Some(PathBuf::from(path.replace('\\', "/")))
}

/// Convert a raw Windows path like `C:\Users\foo\bar.png` to a WSL path.
pub(crate) fn windows_path_to_wsl(path: &str) -> PathBuf {
    let normalized: String = path
        .chars()
        .map(|c| if c == '\\' { '/' } else { c })
        .collect();
    let without_colon = normalized.replacen(':', "", 1);
    let drive = without_colon
        .chars()
        .next()
        .map(|c| c.to_lowercase().to_string())
        .unwrap_or_default();
    if without_colon.len() < 2 {
        return PathBuf::from(normalized);
    }
    PathBuf::from(format!("/mnt/{drive}{}", &without_colon[1..]))
}

/// Minimal percent-decoder for URI path components.
pub fn percent_decode(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(high), Some(low)) = (hex_value(bytes[i + 1]), hex_value(bytes[i + 2])) {
                output.push(((high << 4) | low) as char);
                i += 3;
                continue;
            }
        }
        output.push(bytes[i] as char);
        i += 1;
    }
    output
}

fn hex_value(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'A'..=b'F' => Some(b - b'A' + 10),
        b'a'..=b'f' => Some(b - b'a' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_resolves_drive_uri() {
        let ctx = PlatformContext::Windows;
        assert_eq!(
            ctx.resolve_file_uri("file:///C:/Users/foo/bar.png")
                .unwrap(),
            PathBuf::from("C:\\Users\\foo\\bar.png")
        );
    }

    #[test]
    fn linux_resolves_absolute_uri() {
        let ctx = PlatformContext::Linux;
        assert_eq!(
            ctx.resolve_file_uri("file:///home/user/my file.txt")
                .unwrap(),
            PathBuf::from("/home/user/my file.txt")
        );
    }

    #[test]
    fn wsl_resolves_windows_drive_uri() {
        let ctx = PlatformContext::Wsl;
        assert_eq!(
            ctx.resolve_file_uri("file:///C:/Users/foo/bar.png")
                .unwrap(),
            PathBuf::from("/mnt/c/Users/foo/bar.png")
        );
    }

    #[test]
    fn wsl_resolves_wsl_network_uri() {
        let ctx = PlatformContext::Wsl;
        assert_eq!(
            ctx.resolve_file_uri("file://wsl$/Ubuntu/home/user/foo.txt")
                .unwrap(),
            PathBuf::from("/home/user/foo.txt")
        );
        assert_eq!(
            ctx.resolve_file_uri("file:///wsl.localhost/Ubuntu/home/user/foo.txt")
                .unwrap(),
            PathBuf::from("/home/user/foo.txt")
        );
    }

    #[test]
    fn wsl_resolves_linux_uri() {
        let ctx = PlatformContext::Wsl;
        assert_eq!(
            ctx.resolve_file_uri("file:///home/user/foo.txt").unwrap(),
            PathBuf::from("/home/user/foo.txt")
        );
    }

    #[test]
    fn windows_path_to_wsl_converts() {
        assert_eq!(
            windows_path_to_wsl("C:\\Users\\foo\\bar.png"),
            PathBuf::from("/mnt/c/Users/foo/bar.png")
        );
    }

    #[test]
    fn percent_decode_handles_encoded_space() {
        assert_eq!(percent_decode("my%20file.txt"), "my file.txt");
    }

    #[test]
    fn wsl_parses_raw_windows_path_text() {
        let ctx = PlatformContext::Wsl;
        let paths = ctx
            .parse_file_paths_from_text("C:\\Users\\foo\\bar.png")
            .unwrap();
        assert_eq!(paths, vec![PathBuf::from("/mnt/c/Users/foo/bar.png")]);
    }

    #[test]
    fn wsl_parses_raw_linux_path_text() {
        let ctx = PlatformContext::Wsl;
        let paths = ctx
            .parse_file_paths_from_text("/home/user/foo.txt")
            .unwrap();
        assert_eq!(paths, vec![PathBuf::from("/home/user/foo.txt")]);
    }

    #[test]
    fn linux_parses_raw_unix_path_text() {
        let ctx = PlatformContext::Linux;
        let paths = ctx
            .parse_file_paths_from_text("/home/user/foo.txt")
            .unwrap();
        assert_eq!(paths, vec![PathBuf::from("/home/user/foo.txt")]);
    }

    #[test]
    fn windows_parses_raw_windows_path_text() {
        let ctx = PlatformContext::Windows;
        let paths = ctx
            .parse_file_paths_from_text("C:\\Users\\foo\\bar.png")
            .unwrap();
        assert_eq!(paths, vec![PathBuf::from("C:\\Users\\foo\\bar.png")]);
    }

    #[test]
    fn parse_file_paths_rejects_plain_text() {
        let ctx = PlatformContext::Wsl;
        assert!(ctx.parse_file_paths_from_text("hello world").is_none());
    }

    #[test]
    fn parse_file_paths_handles_mixed_uri_and_raw_path() {
        let ctx = PlatformContext::Wsl;
        let input = "file:///C:/Users/foo/a.txt\r\nC:\\Users\\foo\\b.txt";
        let paths = ctx.parse_file_paths_from_text(input).unwrap();
        assert_eq!(
            paths,
            vec![
                PathBuf::from("/mnt/c/Users/foo/a.txt"),
                PathBuf::from("/mnt/c/Users/foo/b.txt"),
            ]
        );
    }

    #[test]
    fn parse_file_paths_trims_quotes_and_whitespace() {
        let ctx = PlatformContext::Wsl;
        let input = "  \"D:\\workspace\\file.txt\"  ";
        let paths = ctx.parse_file_paths_from_text(input).unwrap();
        assert_eq!(paths, vec![PathBuf::from("/mnt/d/workspace/file.txt")]);
    }

    #[test]
    fn wsl_parses_chinese_windows_path() {
        let ctx = PlatformContext::Wsl;
        let input = "D:\\workspace\\板卡维修\\AutoRework_PitchDeck_v4.pptx";
        let paths = ctx.parse_file_paths_from_text(input).unwrap();
        assert_eq!(
            paths,
            vec![PathBuf::from(
                "/mnt/d/workspace/板卡维修/AutoRework_PitchDeck_v4.pptx"
            )]
        );
    }

    #[test]
    fn format_file_reference_adds_at_for_agent() {
        assert_eq!(
            format_file_reference("/tmp/waitagent/file.png", true),
            "@/tmp/waitagent/file.png"
        );
    }

    #[test]
    fn format_file_reference_quotes_paths_with_spaces() {
        assert_eq!(
            format_file_reference("/tmp/my file.png", true),
            "\"@/tmp/my file.png\""
        );
    }

    #[test]
    fn format_file_reference_returns_raw_path_for_non_agent() {
        assert_eq!(
            format_file_reference("/tmp/waitagent/file.png", false),
            "/tmp/waitagent/file.png"
        );
    }
}
