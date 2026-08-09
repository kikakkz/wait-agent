use std::fs::{read_to_string, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::time::{SystemTime, UNIX_EPOCH};

const LOG_FILE: &str = "/tmp/waitagent-diag.log";
/// Maximum bytes to read from the end of the log file when showing recent entries.
const TAIL_BYTES: usize = 256 * 1024;

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
            LogLevel::Debug => "DBG",
            LogLevel::Info => "INF",
            LogLevel::Warn => "WRN",
            LogLevel::Error => "ERR",
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

pub struct ErrorLog;

impl ErrorLog {
    pub const fn new() -> Self {
        Self
    }

    pub fn log(&self, message: String) {
        self.write(LogLevel::Info, message);
    }

    pub fn log_debug(&self, message: String) {
        self.write(LogLevel::Debug, message);
    }

    pub fn log_warn(&self, message: String) {
        self.write(LogLevel::Warn, message);
    }

    pub fn log_error(&self, message: String) {
        self.write(LogLevel::Error, message);
    }

    pub fn log_exit_latency(&self, message: String) {
        if std::env::var_os("WAITAGENT_EXIT_LATENCY_DIAG").is_some() {
            self.write(LogLevel::Info, message);
        }
    }

    fn write(&self, level: LogLevel, message: String) {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let line = format!(
            "[{}.{:03}] [{}] {}\n",
            ts / 1000,
            ts % 1000,
            level.as_str(),
            message
        );
        let _ = OpenOptions::new()
            .create(true)
            .append(true)
            .open(LOG_FILE)
            .and_then(|mut f| f.write_all(line.as_bytes()));
    }

    /// Read the most recent `max_lines` log entries.
    ///
    /// To avoid freezing the UI when the log file is very large, only the last
    /// [`TAIL_BYTES`] of the file are parsed. If `max_lines` is larger than
    /// the number of entries in that tail, all tail entries are returned.
    pub fn recent_entries(&self, max_lines: usize) -> Vec<(u128, LogLevel, String)> {
        let file = match File::open(LOG_FILE) {
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
        let content = match read_to_string(LOG_FILE) {
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
}
