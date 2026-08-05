//! Local cache for clipboard file/image paste.

use crate::lifecycle::LifecycleError;
use std::path::{Path, PathBuf};

/// Return the directory used to store files pasted from the clipboard.
pub fn clipboard_cache_dir() -> PathBuf {
    std::env::temp_dir().join("waitagent")
}

/// Write `bytes` to the clipboard cache and return the absolute path.
///
/// `filename_hint` is sanitized and a random suffix is appended to avoid
/// collisions. The original extension is preserved when present.
pub fn write_clipboard_file(filename_hint: &str, bytes: &[u8]) -> Result<PathBuf, LifecycleError> {
    let dir = clipboard_cache_dir();
    std::fs::create_dir_all(&dir).map_err(|error| {
        LifecycleError::Io(
            format!("failed to create clipboard cache dir {}", dir.display()),
            error,
        )
    })?;

    let safe_name = unique_cached_filename(filename_hint);
    let path = dir.join(&safe_name);
    std::fs::write(&path, bytes).map_err(|error| {
        LifecycleError::Io(
            format!("failed to write clipboard cache file {}", path.display()),
            error,
        )
    })?;
    Ok(path)
}

/// Generate a sanitized, unique filename for a cached clipboard file.
pub fn unique_cached_filename(filename_hint: &str) -> String {
    let stem = sanitize_filename_stem(filename_hint);
    let extension = extension_from_filename(filename_hint);
    let suffix = random_suffix(8);
    match extension {
        Some(ext) if !ext.is_empty() => format!("{stem}-{suffix}.{ext}"),
        _ => format!("{stem}-{suffix}"),
    }
}

/// Remove path separators, parent-directory references, extension, and control characters.
fn sanitize_filename_stem(name: &str) -> String {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return "paste".to_string();
    }
    let path = Path::new(trimmed);
    let file_stem = path
        .file_stem()
        .and_then(|os| os.to_str())
        .unwrap_or("paste");
    file_stem
        .chars()
        .filter(|c| !c.is_control() && *c != '\0')
        .collect::<String>()
        .trim()
        .to_string()
}

/// Extract the extension from a filename, preserving it for the cached file.
fn extension_from_filename(name: &str) -> Option<String> {
    Path::new(name)
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_lowercase())
}

/// Generate a short random alphanumeric suffix.
fn random_suffix(len: usize) -> String {
    use getrandom::fill;
    const ALPHANUM: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut bytes = vec![0u8; len];
    if fill(&mut bytes).is_err() {
        // Fallback to a timestamp-based suffix if getrandom fails.
        return std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis().to_string())
            .unwrap_or_else(|_| "00000000".to_string());
    }
    bytes
        .into_iter()
        .map(|b| ALPHANUM[b as usize % ALPHANUM.len()] as char)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_stem_removes_path_components_and_extension() {
        assert_eq!(sanitize_filename_stem("/etc/passwd"), "passwd");
        assert_eq!(sanitize_filename_stem("../../secret.txt"), "secret");
    }

    #[test]
    fn unique_cached_filename_preserves_extension() {
        let name = unique_cached_filename("screenshot.png");
        assert!(name.starts_with("screenshot-"));
        assert!(name.ends_with(".png"));
    }

    #[test]
    fn unique_cached_filename_uses_default_when_empty() {
        let name = unique_cached_filename("");
        assert!(name.starts_with("paste-"));
    }

    #[test]
    fn write_clipboard_file_creates_file() {
        let path = write_clipboard_file("test.txt", b"hello cache").expect("write cache file");
        assert!(path.exists());
        assert_eq!(
            std::fs::read(&path).expect("read cache file"),
            b"hello cache"
        );
        let _ = std::fs::remove_file(&path);
    }
}
