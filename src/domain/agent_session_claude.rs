use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use super::agent_session::{AgentSession, AgentSessionError, AgentSessionProvider, ResumeCommand};

/// Resolves the Claude Code data root directory from the environment.
///
/// Resolution order:
/// 1. `$HOME/.claude`
/// 2. `%USERPROFILE%\.claude` (Windows)
/// 3. `.claude` (fallback in current directory)
fn resolve_claude_home() -> Result<PathBuf, AgentSessionError> {
    if let Some(home) = std::env::var_os("HOME") {
        let claude = PathBuf::from(&home).join(".claude");
        if claude.exists() {
            return Ok(claude);
        }
    }
    if let Some(profile) = std::env::var_os("USERPROFILE") {
        let claude = PathBuf::from(&profile).join(".claude");
        if claude.exists() {
            return Ok(claude);
        }
    }
    let local = PathBuf::from(".claude");
    if local.exists() {
        return Ok(local);
    }
    Err(AgentSessionError::HomeNotFound)
}

/// Claude session provider.
///
/// Reads `history.jsonl` at the data root. The file contains one row per user
/// prompt, so rows are deduplicated by `sessionId` and the latest timestamp is
/// kept. If the index is missing or empty, the provider falls back to scanning
/// the `projects/<project>/chat_<uuid>.jsonl` storage tree and reading the
/// first `UserMessage` of each session file.
#[derive(Debug, Clone, Default)]
pub struct ClaudeSessionProvider {
    /// Optional home directory override, used primarily by tests.
    home_override: Option<PathBuf>,
}

impl ClaudeSessionProvider {
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
            .unwrap_or_else(resolve_claude_home)
    }
}

impl AgentSessionProvider for ClaudeSessionProvider {
    fn name(&self) -> &'static str {
        "claude"
    }

    fn list_sessions(&self) -> Result<Vec<AgentSession>, AgentSessionError> {
        let home = self.home()?;
        let index_path = home.join("history.jsonl");

        if index_path.exists() {
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

        let projects_dir = home.join("projects");
        if projects_dir.exists() {
            Ok(list_sessions_from_projects(&projects_dir))
        } else {
            Ok(Vec::new())
        }
    }

    fn resume_command(&self, session: &AgentSession) -> ResumeCommand {
        ResumeCommand {
            program: "claude".into(),
            args: vec!["-r".into(), session.id.clone()],
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
        .get("sessionId")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())?;

    let timestamp_ms = value.get("timestamp").and_then(parse_timestamp_ms)?;

    let title = value
        .get("display")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let cwd = value
        .get("project")
        .and_then(|v| v.as_str())
        .map(PathBuf::from);

    Some(AgentSession {
        id,
        title,
        last_prompt: None,
        cwd,
        updated_at: Some(SystemTime::UNIX_EPOCH + Duration::from_millis(timestamp_ms)),
    })
}

fn parse_timestamp_ms(value: &serde_json::Value) -> Option<u64> {
    if let Some(n) = value.as_u64() {
        return Some(n);
    }
    if let Some(s) = value.as_str() {
        return s.parse().ok();
    }
    None
}

fn list_sessions_from_projects(projects_dir: &Path) -> Vec<AgentSession> {
    let mut sessions = Vec::new();

    visit_session_files(projects_dir, &mut |path| {
        if let Some(session) = parse_session_file(path) {
            sessions.push(session);
        }
    });

    sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    sessions
}

fn visit_session_files(dir: &Path, visitor: &mut dyn FnMut(&Path)) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            visit_session_files(&path, visitor);
        } else {
            let is_session_file = path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("jsonl"))
                && path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .is_some_and(|s| extract_uuid(s).is_some());
            if is_session_file {
                visitor(&path);
            }
        }
    }
}

fn parse_session_file(path: &Path) -> Option<AgentSession> {
    let file = fs::File::open(path).ok()?;
    let reader = BufReader::new(file);

    for line in reader.lines().take(200).flatten() {
        if let Some(session) = parse_session_line(&line) {
            return Some(session);
        }
    }

    // If no UserMessage was found, derive what we can from the filename and
    // file metadata so the session is still resumable.
    let id = extract_uuid(path.file_stem().and_then(|s| s.to_str())?)?;
    let updated_at = fs::metadata(path).ok()?.modified().ok()?;
    Some(AgentSession {
        id,
        title: None,
        last_prompt: None,
        cwd: None,
        updated_at: Some(updated_at),
    })
}

fn parse_session_line(line: &str) -> Option<AgentSession> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;

    if value.get("type").and_then(|v| v.as_str()) != Some("user") {
        return None;
    }

    let id = value
        .get("sessionId")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())?;

    let updated_at = value
        .get("timestamp")
        .and_then(|v| v.as_str())
        .and_then(parse_iso8601_utc_to_system_time)
        .or_else(|| {
            value
                .get("timestamp")
                .and_then(parse_timestamp_ms)
                .map(|ms| SystemTime::UNIX_EPOCH + Duration::from_millis(ms))
        })?;

    let cwd = value.get("cwd").and_then(|v| v.as_str()).map(PathBuf::from);

    let title = value
        .get("slug")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| extract_first_text_content(&value));

    Some(AgentSession {
        id,
        title,
        last_prompt: None,
        cwd,
        updated_at: Some(updated_at),
    })
}

fn extract_first_text_content(value: &serde_json::Value) -> Option<String> {
    let content = value.get("message")?.get("content")?;

    if let Some(text) = content.as_str() {
        return Some(text.to_string());
    }

    content
        .as_array()?
        .iter()
        .find(|block| block.get("type").and_then(|v| v.as_str()) == Some("text"))
        .and_then(|block| block.get("text").and_then(|v| v.as_str()))
        .map(|s| s.to_string())
}

fn extract_uuid(stem: &str) -> Option<String> {
    // Accepts stems like `<uuid>` or `chat_<uuid>`.
    let candidate = stem
        .strip_prefix("chat_")
        .or_else(|| stem.strip_prefix("agent-"))
        .unwrap_or(stem);
    if looks_like_uuid(candidate) {
        return Some(candidate.to_string());
    }
    None
}

fn looks_like_uuid(text: &str) -> bool {
    if text.len() != 36 {
        return false;
    }
    let bytes = text.as_bytes();
    for (idx, &b) in bytes.iter().enumerate() {
        let expected_dash = idx == 8 || idx == 13 || idx == 18 || idx == 23;
        if expected_dash {
            if b != b'-' {
                return false;
            }
        } else if !b.is_ascii_hexdigit() {
            return false;
        }
    }
    true
}

/// Parses ISO-8601 UTC timestamps of the form `YYYY-MM-DDTHH:MM:SS[.fff]Z`
/// into `SystemTime`.
///
/// This intentionally narrow parser covers the format produced by Claude Code
/// session files without pulling in a full date-time library.
fn parse_iso8601_utc_to_system_time(text: &str) -> Option<SystemTime> {
    let text = text.strip_suffix('Z')?;
    let (date_part, time_part) = text.split_once('T')?;

    let mut date = date_part.split('-');
    let year: i32 = date.next()?.parse().ok()?;
    let month: u32 = date.next()?.parse().ok()?;
    let day: u32 = date.next()?.parse().ok()?;

    let mut time = time_part.split(':');
    let hour: u32 = time.next()?.parse().ok()?;
    let minute: u32 = time.next()?.parse().ok()?;
    let second_and_frac = time.next()?;

    let (second, millis) = if let Some((sec, frac)) = second_and_frac.split_once('.') {
        let sec: u32 = sec.parse().ok()?;
        let frac = format!("{:0<3}", &frac[..frac.len().min(3)])[..3].to_string();
        let millis: u32 = frac.parse().ok()?;
        (sec, millis)
    } else {
        (second_and_frac.parse().ok()?, 0)
    };

    let days_since_epoch = days_since_unix_epoch(year, month, day)?;
    let secs =
        days_since_epoch as u64 * 86400 + hour as u64 * 3600 + minute as u64 * 60 + second as u64;

    Some(SystemTime::UNIX_EPOCH + Duration::from_secs(secs) + Duration::from_millis(millis as u64))
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
        std::env::temp_dir().join(format!("waitagent-test-claude-{nanos}"))
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
                r#"{"display":"first","timestamp":1759146405000,"project":"/tmp/a","sessionId":"aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"}"#,
                r#"{"display":"second","timestamp":1759146406000,"project":"/tmp/a","sessionId":"aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"}"#,
                r#"{"display":"other","timestamp":1759146407000,"project":"/tmp/b","sessionId":"bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb"}"#,
            ],
        );

        let provider = ClaudeSessionProvider::with_home(home.clone());
        let sessions = provider.list_sessions().unwrap();
        cleanup(&home);

        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].id, "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb");
        assert_eq!(sessions[0].title.as_deref(), Some("other"));
        assert_eq!(sessions[0].cwd, Some(PathBuf::from("/tmp/b")));
        assert_eq!(sessions[1].id, "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa");
        assert_eq!(sessions[1].title.as_deref(), Some("second"));
    }

    #[test]
    fn list_sessions_skips_malformed_lines() {
        let home = make_temp_home();
        write_history(
            &home,
            &[
                "not json",
                r#"{"display":"ok","timestamp":1759146405000,"project":"/tmp/c","sessionId":"cccccccc-cccc-cccc-cccc-cccccccccccc"}"#,
            ],
        );

        let provider = ClaudeSessionProvider::with_home(home.clone());
        let sessions = provider.list_sessions().unwrap();
        cleanup(&home);

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "cccccccc-cccc-cccc-cccc-cccccccccccc");
    }

    #[test]
    fn list_sessions_sorts_by_timestamp_descending() {
        let home = make_temp_home();
        write_history(
            &home,
            &[
                r#"{"display":"old","timestamp":1000,"project":"/tmp/old","sessionId":"00000000-0000-0000-0000-000000000001"}"#,
                r#"{"display":"new","timestamp":2000,"project":"/tmp/new","sessionId":"00000000-0000-0000-0000-000000000002"}"#,
            ],
        );

        let provider = ClaudeSessionProvider::with_home(home.clone());
        let sessions = provider.list_sessions().unwrap();
        cleanup(&home);

        assert_eq!(sessions[0].id, "00000000-0000-0000-0000-000000000002");
        assert_eq!(sessions[1].id, "00000000-0000-0000-0000-000000000001");
    }

    #[test]
    fn list_sessions_falls_back_to_project_scan() {
        let home = make_temp_home();
        let project_dir = home.join("projects").join("-tmp-demo");
        fs::create_dir_all(&project_dir).unwrap();
        fs::write(
            project_dir.join("chat_dddddddd-dddd-dddd-dddd-dddddddddddd.jsonl"),
            serde_json::json!({
                "type": "user",
                "sessionId": "dddddddd-dddd-dddd-dddd-dddddddddddd",
                "timestamp": "2026-07-13T09:21:28.024Z",
                "cwd": "/tmp/demo",
                "slug": "demo session",
                "message": { "role": "user", "content": [{"type": "text", "text": "hello"}] },
            })
            .to_string()
                + "\n",
        )
        .unwrap();

        let provider = ClaudeSessionProvider::with_home(home.clone());
        let sessions = provider.list_sessions().unwrap();
        cleanup(&home);

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "dddddddd-dddd-dddd-dddd-dddddddddddd");
        assert_eq!(sessions[0].title.as_deref(), Some("demo session"));
        assert_eq!(sessions[0].cwd, Some(PathBuf::from("/tmp/demo")));
        assert!(sessions[0].updated_at.is_some());
    }

    #[test]
    fn resume_command_format() {
        let session = AgentSession {
            id: "sess-x".into(),
            title: None,
            last_prompt: None,
            cwd: None,
            updated_at: None,
        };
        let provider = ClaudeSessionProvider::new();
        let cmd = provider.resume_command(&session);
        assert_eq!(cmd.program, "claude");
        assert_eq!(cmd.args, vec!["-r", "sess-x"]);
    }
}
