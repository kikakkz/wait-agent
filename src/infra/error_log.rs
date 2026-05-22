use std::fs::{read_to_string, OpenOptions};
use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

const LOG_FILE: &str = "/tmp/waitagent-diag.log";

pub struct ErrorLog;

impl ErrorLog {
    pub const fn new() -> Self {
        Self
    }

    pub fn log(&self, message: String) {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let line = format!("[{}.{:03}] {}\n", ts / 1000, ts % 1000, message);
        let _ = OpenOptions::new()
            .create(true)
            .append(true)
            .open(LOG_FILE)
            .and_then(|mut f| f.write_all(line.as_bytes()));
    }

    pub fn entries(&self) -> Vec<(u128, String)> {
        let content = match read_to_string(LOG_FILE) {
            Ok(c) => c,
            Err(_) => return Vec::new(),
        };
        content
            .lines()
            .filter_map(|line| {
                if line.len() < 14 || !line.starts_with('[') {
                    return None;
                }
                let secs: u128 = line[1..11].parse().ok()?;
                let millis: u128 = line[12..15].parse().ok()?;
                let ts = secs * 1000 + millis;
                let msg = line[16..].to_string();
                Some((ts, msg))
            })
            .collect()
    }
}

pub static ERROR_LOG: ErrorLog = ErrorLog::new();
