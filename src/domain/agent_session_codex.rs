use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use super::agent_session::{AgentSession, AgentSessionError, AgentSessionProvider, ResumeCommand};

/// Resolves the Codex data root directory from the environment.
///
/// Resolution order:
/// 1. `CODEX_HOME`
/// 2. `$HOME/.codex`
/// 3. `.codex` (fallback in current directory)
fn resolve_codex_home() -> Result<PathBuf, AgentSessionError> {
    if let Some(dir) = std::env::var_os("CODEX_HOME") {
        return Ok(PathBuf::from(dir));
    }
    if let Some(home) = std::env::var_os("HOME") {
        let codex = PathBuf::from(&home).join(".codex");
        if codex.exists() {
            return Ok(codex);
        }
    }
    let local = PathBuf::from(".codex");
    if local.exists() {
        return Ok(local);
    }
    Err(AgentSessionError::HomeNotFound)
}

/// Codex session provider.
///
/// Reads `history.jsonl` at the data root. The index contains one row per
/// conversation turn, so rows are deduplicated by `session_id` and the latest
/// `ts` is kept. If the index is missing, the provider falls back to scanning
/// the `sessions/YYYY/MM/DD/rollout-*.jsonl` storage tree.
#[derive(Debug, Clone, Default)]
pub struct CodexSessionProvider {
    /// Optional home directory override, used primarily by tests.
    home_override: Option<PathBuf>,
}

impl CodexSessionProvider {
    pub fn new() -> Self {
        Self {
            home_override: None,
        }
    }

    #[cfg(test)]
    pub fn with_home(home: PathBuf) -> Self {
        Self {
            home_override: Some(home),
        }
    }

    fn home(&self) -> Result<PathBuf, AgentSessionError> {
        self.home_override
            .clone()
            .map(Ok)
            .unwrap_or_else(resolve_codex_home)
    }
}

impl AgentSessionProvider for CodexSessionProvider {
    fn name(&self) -> &'static str {
        "codex"
    }

    fn list_sessions(&self) -> Result<Vec<AgentSession>, AgentSessionError> {
        let home = self.home()?;
        let index_path = home.join("history.jsonl");

        let index_missing = !index_path.exists();
        if !index_missing {
            match fs::read_to_string(&index_path) {
                Ok(content) => {
                    let sessions = list_sessions_from_history(&content);
                    if !sessions.is_empty() {
                        return Ok(sessions);
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(AgentSessionError::IndexRead(error)),
            }
        }

        let sessions_dir = home.join("sessions");
        if sessions_dir.exists() {
            Ok(list_sessions_from_rollouts(&sessions_dir))
        } else {
            Ok(Vec::new())
        }
    }

    fn resume_command(&self, session: &AgentSession) -> ResumeCommand {
        ResumeCommand {
            program: "codex".into(),
            args: vec!["resume".into(), session.id.clone()],
        }
    }
}

fn list_sessions_from_history(content: &str) -> Vec<AgentSession> {
    let mut by_id: HashMap<String, AgentSession> = HashMap::new();

    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Some(session) = parse_history_line(line) {
            by_id
                .entry(session.id.clone())
                .and_modify(|existing| {
                    if session.updated_at > existing.updated_at {
                        *existing = session.clone();
                    }
                })
                .or_insert(session);
        }
    }

    let mut sessions: Vec<AgentSession> = by_id.into_values().collect();
    sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    sessions
}

fn parse_history_line(line: &str) -> Option<AgentSession> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;

    let id = value
        .get("session_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())?;

    let ts_secs = value.get("ts").and_then(|v| v.as_u64())?;

    let title = value
        .get("text")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    Some(AgentSession {
        id,
        title,
        cwd: None,
        updated_at: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(ts_secs)),
    })
}

fn list_sessions_from_rollouts(sessions_dir: &Path) -> Vec<AgentSession> {
    let mut sessions = Vec::new();
    let mut failed = 0usize;

    visit_rollout_files(sessions_dir, &mut |path| match parse_rollout_path(path) {
        Some(session) => sessions.push(session),
        None => failed += 1,
    });

    sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    sessions
}

fn visit_rollout_files(dir: &Path, visitor: &mut dyn FnMut(&Path)) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            visit_rollout_files(&path, visitor);
        } else if path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("rollout-") && n.ends_with(".jsonl"))
        {
            visitor(&path);
        }
    }
}

fn parse_rollout_path(path: &Path) -> Option<AgentSession> {
    let name = path.file_name().and_then(|n| n.to_str())?;
    let stem = name.strip_suffix(".jsonl")?;
    let prefix = "rollout-";
    let body = stem.strip_prefix(prefix)?;

    // UUID is the final 36 characters (including its dashes).
    if body.len() < 37 {
        return None;
    }
    let (timestamp_part, uuid_with_leading_dash) = body.split_at(body.len() - 37);
    let id = uuid_with_leading_dash.strip_prefix('-')?.to_string();

    let updated_at = parse_rollout_timestamp(timestamp_part)
        .or_else(|| fs::metadata(path).ok()?.modified().ok())?;

    Some(AgentSession {
        id,
        title: None,
        cwd: None,
        updated_at: Some(updated_at),
    })
}

/// Parses the timestamp portion of a rollout filename.
///
/// Supports two observed forms:
/// - Unix epoch seconds: `rollout-<seconds>-<uuid>.jsonl`
/// - ISO-like local timestamp: `rollout-<YYYY-MM-DDTHH-MM-SS>-<uuid>.jsonl`
fn parse_rollout_timestamp(timestamp_part: &str) -> Option<SystemTime> {
    if timestamp_part.chars().all(|c| c.is_ascii_digit()) {
        let secs = timestamp_part.parse::<u64>().ok()?;
        return Some(SystemTime::UNIX_EPOCH + Duration::from_secs(secs));
    }

    parse_iso_local_timestamp(timestamp_part)
}

/// Parses `YYYY-MM-DDTHH-MM-SS` (where the time separators are dashes instead
/// of colons, as used in Codex rollout filenames) into a UTC `SystemTime`.
fn parse_iso_local_timestamp(text: &str) -> Option<SystemTime> {
    if text.len() != "YYYY-MM-DDTHH-MM-SS".len() {
        return None;
    }

    let year: i32 = text[..4].parse().ok()?;
    let _dash1 = text.as_bytes().get(4).filter(|&&b| b == b'-')?;
    let month: u32 = text[5..7].parse().ok()?;
    let _dash2 = text.as_bytes().get(7).filter(|&&b| b == b'-')?;
    let day: u32 = text[8..10].parse().ok()?;
    let _t = text.as_bytes().get(10).filter(|&&b| b == b'T')?;
    let hour: u32 = text[11..13].parse().ok()?;
    let _dash3 = text.as_bytes().get(13).filter(|&&b| b == b'-')?;
    let minute: u32 = text[14..16].parse().ok()?;
    let _dash4 = text.as_bytes().get(16).filter(|&&b| b == b'-')?;
    let second: u32 = text[17..19].parse().ok()?;

    let days = days_since_unix_epoch(year, month, day)?;
    let secs = days as u64 * 86400 + hour as u64 * 3600 + minute as u64 * 60 + second as u64;
    Some(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
}

fn days_since_unix_epoch(year: i32, month: u32, day: u32) -> Option<i64> {
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }

    let mut days: i64 = 0;
    let mut y = 1970;
    while y < year {
        days += if is_leap_year(y) { 366 } else { 365 };
        y += 1;
    }
    while y > year {
        y -= 1;
        days -= if is_leap_year(y) { 366 } else { 365 };
    }

    let month_lengths = [
        31,
        if is_leap_year(year) { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    for m in 1..month {
        days += month_lengths[m as usize - 1] as i64;
    }
    days += day as i64 - 1;

    Some(days)
}

fn is_leap_year(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn make_temp_home() -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("waitagent-test-codex-{nanos}"))
    }

    fn cleanup(home: &Path) {
        let _ = fs::remove_dir_all(home);
    }

    fn write_history(home: &Path, lines: &[&str]) {
        fs::create_dir_all(home).unwrap();
        let mut file = fs::File::create(home.join("history.jsonl")).unwrap();
        for line in lines {
            writeln!(file, "{line}").unwrap();
        }
    }

    #[test]
    fn list_sessions_reads_history_and_dedupes() {
        let home = make_temp_home();
        write_history(
            &home,
            &[
                r#"{"session_id":"sess-a","ts":1782219826,"text":"first prompt"}"#,
                r#"{"session_id":"sess-b","ts":1782219900,"text":"other prompt"}"#,
                r#"{"session_id":"sess-a","ts":1782220000,"text":" resumed prompt"}"#,
            ],
        );

        let provider = CodexSessionProvider::with_home(home.clone());
        let sessions = provider.list_sessions().unwrap();
        cleanup(&home);

        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].id, "sess-a");
        assert_eq!(sessions[0].title.as_deref(), Some(" resumed prompt"));
        assert_eq!(sessions[1].id, "sess-b");
    }

    #[test]
    fn list_sessions_skips_malformed_lines() {
        let home = make_temp_home();
        write_history(
            &home,
            &[
                "not json",
                r#"{"session_id":"sess-c","ts":1782219826,"text":"ok"}"#,
            ],
        );

        let provider = CodexSessionProvider::with_home(home.clone());
        let sessions = provider.list_sessions().unwrap();
        cleanup(&home);

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "sess-c");
    }

    #[test]
    fn list_sessions_sorts_by_timestamp_descending() {
        let home = make_temp_home();
        write_history(
            &home,
            &[
                r#"{"session_id":"old","ts":1000,"text":"old"}"#,
                r#"{"session_id":"new","ts":2000,"text":"new"}"#,
            ],
        );

        let provider = CodexSessionProvider::with_home(home.clone());
        let sessions = provider.list_sessions().unwrap();
        cleanup(&home);

        assert_eq!(sessions[0].id, "new");
        assert_eq!(sessions[1].id, "old");
    }

    #[test]
    fn list_sessions_falls_back_to_rollout_scan() {
        let home = make_temp_home();
        let rollout_dir = home.join("sessions").join("2026").join("07").join("13");
        fs::create_dir_all(&rollout_dir).unwrap();
        fs::write(
            rollout_dir
                .join("rollout-2026-07-13T09-21-28-019e029c-b1e9-7e31-992e-df4638cf8ee8.jsonl"),
            "{}",
        )
        .unwrap();

        let provider = CodexSessionProvider::with_home(home.clone());
        let sessions = provider.list_sessions().unwrap();
        cleanup(&home);

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "019e029c-b1e9-7e31-992e-df4638cf8ee8");
        assert!(sessions[0].updated_at.is_some());
    }

    #[test]
    fn resume_command_format() {
        let session = AgentSession {
            id: "sess-x".into(),
            title: None,
            cwd: None,
            updated_at: None,
        };
        let provider = CodexSessionProvider::new();
        let cmd = provider.resume_command(&session);
        assert_eq!(cmd.program, "codex");
        assert_eq!(cmd.args, vec!["resume", "sess-x"]);
    }
}
