use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_ENTRIES: usize = 500;

#[derive(Clone)]
struct LogEntry {
    timestamp: u128,
    message: String,
}

pub struct ErrorLog {
    entries: Mutex<Vec<LogEntry>>,
}

impl ErrorLog {
    pub const fn new() -> Self {
        Self {
            entries: Mutex::new(Vec::new()),
        }
    }

    pub fn log(&self, message: String) {
        let mut entries = match self.entries.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        entries.push(LogEntry {
            timestamp: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis(),
            message,
        });
        while entries.len() > MAX_ENTRIES {
            entries.remove(0);
        }
    }

    pub fn entries(&self) -> Vec<(u128, String)> {
        match self.entries.lock() {
            Ok(guard) => guard
                .iter()
                .map(|e| (e.timestamp, e.message.clone()))
                .collect(),
            Err(poisoned) => poisoned
                .into_inner()
                .iter()
                .map(|e| (e.timestamp, e.message.clone()))
                .collect(),
        }
    }
}

pub static ERROR_LOG: ErrorLog = ErrorLog::new();
