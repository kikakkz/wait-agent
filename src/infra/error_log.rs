use std::fs::{read_to_string, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// Path of the diagnostics log file. Fixed location on Unix (existing
/// tooling reads `/tmp/waitagent-diag.log`); the system temp dir elsewhere
/// so logging works on Windows too.
fn log_file() -> std::path::PathBuf {
    #[cfg(unix)]
    {
        std::path::PathBuf::from("/tmp/waitagent-diag.log")
    }
    #[cfg(not(unix))]
    {
        std::env::temp_dir().join("waitagent-diag.log")
    }
}
/// Maximum bytes to read from the end of the log file when showing recent entries.
const TAIL_BYTES: usize = 256 * 1024;

/// Maximum size of the diagnostics log before it is truncated. Bounded so a
/// log-spam loop cannot grow the file without limit and slow every write.
const MAX_LOG_BYTES: u64 = 16 * 1024 * 1024;

/// Last message seen by [`ErrorLog`], used to suppress unbounded repeats.
struct LastEntry {
    message: String,
    count: u64,
}

static LAST: OnceLock<Mutex<LastEntry>> = OnceLock::new();

fn last_entry() -> &'static Mutex<LastEntry> {
    LAST.get_or_init(|| {
        Mutex::new(LastEntry {
            message: String::new(),
            count: 0,
        })
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

impl LogLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            LogLevel::Debug => "DEBUG",
            LogLevel::Info => "INFO",
            LogLevel::Warn => "WARN",
            LogLevel::Error => "ERROR",
        }
    }

    pub fn short(self) -> &'static str {
        match self {
            LogLevel::Debug => "D",
            LogLevel::Info => "I",
            LogLevel::Warn => "W",
            LogLevel::Error => "E",
        }
    }

    fn from_str(s: &str) -> Option<Self> {
        match s {
            "DEBUG" | "DBG" => Some(LogLevel::Debug),
            "INFO" | "INF" => Some(LogLevel::Info),
            "WARN" | "WRN" => Some(LogLevel::Warn),
            "ERROR" | "ERR" => Some(LogLevel::Error),
            _ => None,
        }
    }
}

/// Decide whether a log write at `current_len` should first truncate the file.
fn should_truncate(current_len: u64) -> bool {
    current_len > MAX_LOG_BYTES
}

/// Format the lines that a new `message` produces given the suppression state.
///
/// Returns `None` when the message is a consecutive repeat (the state counter
/// is bumped and nothing is written). Otherwise returns the lines to append:
/// an optional "... repeated N times" summary of the previous message followed
/// by the new message itself.
fn suppressible_lines(
    last: &mut LastEntry,
    level: LogLevel,
    ts: u128,
    message: &str,
) -> Option<Vec<String>> {
    let format_line = |text: &str| {
        format!(
            "[{}.{:03}] [{}] {}\n",
            ts / 1000,
            ts % 1000,
            level.as_str(),
            text
        )
    };
    if last.message == message {
        last.count += 1;
        return None;
    }
    let mut lines = Vec::with_capacity(2);
    if last.count > 0 {
        lines.push(format_line(&format!(
            "... previous message repeated {} times: {}",
            last.count, last.message
        )));
    }
    lines.push(format_line(message));
    last.message.clear();
    last.message.push_str(message);
    last.count = 0;
    Some(lines)
}

pub struct ErrorLog;

impl ErrorLog {
    pub const fn new() -> Self {
        Self
    }

    pub fn log(&self, message: String) {
        self.write(LogLevel::Info, message);
    }

    pub fn log_warn(&self, message: String) {
        self.write(LogLevel::Warn, message);
    }

    pub fn log_error(&self, message: String) {
        self.write(LogLevel::Error, message);
    }

    fn write(&self, level: LogLevel, message: String) {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();

        // Suppress consecutive repeats inside the mutex (no I/O held), then do
        // the file append after releasing it. The mutex is a leaf: no other
        // lock is acquired while it is held.
        let lines = {
            let mut last = last_entry().lock().unwrap_or_else(|e| e.into_inner());
            suppressible_lines(&mut last, level, ts, &message)
        };
        let Some(lines) = lines else {
            return;
        };

        if let Ok(file) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_file())
        {
            // Rotate by truncating when the log grows past the cap. Dropping
            // old history is acceptable for a diagnostics log and keeps the
            // tail-based reader cheap.
            let mut file = file;
            if file
                .metadata()
                .map(|metadata| should_truncate(metadata.len()))
                .unwrap_or(false)
            {
                let _ = file.set_len(0);
            }
            let mut buf = String::with_capacity(lines.iter().map(|line| line.len()).sum());
            for line in &lines {
                buf.push_str(line);
            }
            let _ = file.write_all(buf.as_bytes());
        }
    }

    /// Read the most recent `max_lines` log entries.
    ///
    /// To avoid freezing the UI when the log file is very large, only the last
    /// [`TAIL_BYTES`] of the file are parsed. If `max_lines` is larger than
    /// the number of entries in that tail, all tail entries are returned.
    pub fn recent_entries(&self, max_lines: usize) -> Vec<(u128, LogLevel, String)> {
        let file = match File::open(log_file()) {
            Ok(f) => f,
            Err(_) => return Vec::new(),
        };
        let metadata = match file.metadata() {
            Ok(m) => m,
            Err(_) => return Vec::new(),
        };
        let file_size = metadata.len();
        let start = file_size.saturating_sub(TAIL_BYTES as u64);

        let mut file = file;
        let _ = file.seek(SeekFrom::Start(start));
        let mut buf = Vec::with_capacity(TAIL_BYTES);
        if file.read_to_end(&mut buf).is_err() {
            return Vec::new();
        }

        let text = if start > 0 {
            // We started mid-file; drop the first (likely partial) line.
            match buf.iter().position(|&b| b == b'\n') {
                Some(idx) => std::str::from_utf8(&buf[idx + 1..]).unwrap_or(""),
                None => "",
            }
        } else {
            std::str::from_utf8(&buf).unwrap_or("")
        };

        let mut entries: Vec<_> = text.lines().filter_map(parse_entry).collect();
        if entries.len() > max_lines {
            entries = entries.split_off(entries.len() - max_lines);
        }
        entries
    }

    /// Read every log entry.
    ///
    /// Prefer [`Self::recent_entries`] for UI use; this reads the entire file
    /// and may be slow for large logs.
    pub fn entries(&self) -> Vec<(u128, LogLevel, String)> {
        let content = match read_to_string(log_file()) {
            Ok(c) => c,
            Err(_) => return Vec::new(),
        };
        content.lines().filter_map(parse_entry).collect()
    }
}

fn parse_entry(line: &str) -> Option<(u128, LogLevel, String)> {
    if line.len() < 14 || !line.starts_with('[') {
        return None;
    }
    let secs: u128 = line[1..11].parse().ok()?;
    let millis: u128 = line[12..15].parse().ok()?;
    let ts = secs * 1000 + millis;

    let after_ts = line[16..].trim_start();
    // New format: [ts.millis] [LEVEL] message
    if after_ts.starts_with('[') {
        let close = after_ts.find(']')?;
        let level_str = &after_ts[1..close];
        let level = LogLevel::from_str(level_str).unwrap_or(LogLevel::Info);
        let msg = after_ts[close + 1..].trim_start().to_string();
        return Some((ts, level, msg));
    }

    // Legacy format: [ts.millis] message
    let msg = line[16..].trim_start().to_string();
    Some((ts, LogLevel::Info, msg))
}

pub static ERROR_LOG: ErrorLog = ErrorLog::new();

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_entry_extracts_timestamp_and_message() {
        let line = "[1234567890.123] [INFO] hello world";
        let (ts, level, msg) = parse_entry(line).unwrap();
        assert_eq!(ts, 1234567890_123);
        assert_eq!(level, LogLevel::Info);
        assert_eq!(msg, "hello world");
    }

    #[test]
    fn parse_entry_rejects_short_line() {
        assert!(parse_entry("[123] hi").is_none());
    }

    #[test]
    fn parse_entry_falls_back_to_info_for_legacy_format() {
        let line = "[1234567890.123] hello world";
        let (ts, level, msg) = parse_entry(line).unwrap();
        assert_eq!(ts, 1234567890_123);
        assert_eq!(level, LogLevel::Info);
        assert_eq!(msg, "hello world");
    }

    #[test]
    fn parse_entry_recognizes_all_levels() {
        assert_eq!(
            parse_entry("[1234567890.000] [DEBUG] x").unwrap().1,
            LogLevel::Debug
        );
        assert_eq!(
            parse_entry("[1234567890.000] [WARN] x").unwrap().1,
            LogLevel::Warn
        );
        assert_eq!(
            parse_entry("[1234567890.000] [ERROR] x").unwrap().1,
            LogLevel::Error
        );
    }

    #[test]
    fn suppressible_lines_drops_consecutive_repeats() {
        let mut last = LastEntry {
            message: String::new(),
            count: 0,
        };
        assert!(suppressible_lines(&mut last, LogLevel::Info, 1000, "offline node-a").is_some());
        assert!(suppressible_lines(&mut last, LogLevel::Info, 1001, "offline node-a").is_none());
        assert!(suppressible_lines(&mut last, LogLevel::Info, 1002, "offline node-a").is_none());
        assert_eq!(last.count, 2);

        let lines = suppressible_lines(&mut last, LogLevel::Info, 1003, "offline node-b")
            .expect("new message must be written");
        assert_eq!(lines.len(), 2);
        assert!(
            lines[0].contains("repeated 2 times"),
            "repeat summary missing: {}",
            lines[0]
        );
        assert!(lines[0].contains("offline node-a"));
        assert!(lines[1].contains("offline node-b"));
    }

    #[test]
    fn suppressible_lines_resets_counter_after_flush() {
        let mut last = LastEntry {
            message: String::new(),
            count: 0,
        };
        let _ = suppressible_lines(&mut last, LogLevel::Info, 1000, "msg-x");
        let _ = suppressible_lines(&mut last, LogLevel::Info, 1001, "msg-x");
        let _ = suppressible_lines(&mut last, LogLevel::Info, 1002, "msg-y");
        assert_eq!(last.count, 0);
        assert_eq!(last.message, "msg-y");
        assert!(suppressible_lines(&mut last, LogLevel::Info, 1003, "msg-y").is_none());
        assert_eq!(last.count, 1);
    }

    #[test]
    fn should_truncate_only_past_cap() {
        assert!(!should_truncate(MAX_LOG_BYTES));
        assert!(should_truncate(MAX_LOG_BYTES + 1));
    }
}
