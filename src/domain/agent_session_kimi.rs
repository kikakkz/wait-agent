use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use super::agent_session::{AgentSession, AgentSessionError, AgentSessionProvider, ResumeCommand};

/// Resolves the Kimi Code data root directory from the environment.
///
/// Resolution order:
/// 1. `KIMI_CODE_HOME` (documented official variable)
/// 2. `KIMI_HOME` (used by existing hook config service)
/// 3. `$HOME/.kimi-code`
/// 4. `$HOME/.kimi` (legacy)
/// 5. `.kimi-code` (fallback in current directory)
fn resolve_kimi_home() -> Result<PathBuf, AgentSessionError> {
    if let Some(dir) = std::env::var_os("KIMI_CODE_HOME") {
        return Ok(PathBuf::from(dir));
    }
    if let Some(dir) = std::env::var_os("KIMI_HOME") {
        return Ok(PathBuf::from(dir));
    }
    if let Some(home) = std::env::var_os("HOME") {
        let kimi_code = PathBuf::from(&home).join(".kimi-code");
        if kimi_code.exists() {
            return Ok(kimi_code);
        }
        let kimi_legacy = PathBuf::from(&home).join(".kimi");
        if kimi_legacy.exists() {
            return Ok(kimi_legacy);
        }
        return Ok(kimi_code);
    }
    Err(AgentSessionError::HomeNotFound)
}

/// Kimi session provider.
///
/// Reads `session_index.jsonl` at the data root and per-session `state.json`
/// files to build a list of resumable sessions.
#[derive(Debug, Clone, Default)]
pub struct KimiSessionProvider {
    /// Optional home directory override, used primarily by tests.
    home_override: Option<PathBuf>,
}

impl KimiSessionProvider {
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
            .unwrap_or_else(resolve_kimi_home)
    }
}

impl AgentSessionProvider for KimiSessionProvider {
    fn name(&self) -> &'static str {
        "kimi"
    }

    fn list_sessions(&self) -> Result<Vec<AgentSession>, AgentSessionError> {
        let home = self.home()?;
        let index_path = home.join("session_index.jsonl");
        let content = fs::read_to_string(&index_path).map_err(AgentSessionError::IndexRead)?;

        let mut sessions = Vec::new();
        let mut failed = 0usize;

        for line in content.lines() {
            if line.trim().is_empty() {
                continue;
            }
            match parse_index_line(line, &home) {
                Some(session) => sessions.push(session),
                None => failed += 1,
            }
        }

        if sessions.is_empty() && failed > 0 {
            return Err(AgentSessionError::Parse(format!(
                "{} lines could not be parsed",
                failed
            )));
        }

        sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        Ok(sessions)
    }

    fn resume_command(&self, session: &AgentSession) -> ResumeCommand {
        ResumeCommand {
            program: "kimi".into(),
            args: vec!["--session".into(), session.id.clone()],
        }
    }
}

fn parse_index_line(line: &str, home: &Path) -> Option<AgentSession> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;

    let id = value
        .get("sessionId")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())?;

    let session_dir = value
        .get("sessionDir")
        .and_then(|v| v.as_str())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join("sessions").join(&id));

    let cwd = value
        .get("workDir")
        .and_then(|v| v.as_str())
        .map(PathBuf::from);

    let mut session = AgentSession {
        id,
        title: None,
        cwd,
        updated_at: None,
    };

    if let Ok(state) = fs::read_to_string(session_dir.join("state.json")) {
        if let Ok(state_value) = serde_json::from_str::<serde_json::Value>(&state) {
            session.title = state_value
                .get("title")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .or_else(|| {
                    state_value
                        .get("lastPrompt")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                });

            session.updated_at = state_value
                .get("updatedAt")
                .and_then(|v| v.as_str())
                .and_then(parse_iso8601_utc_to_system_time);

            if session.cwd.is_none() {
                session.cwd = state_value
                    .get("workDir")
                    .and_then(|v| v.as_str())
                    .map(PathBuf::from);
            }
        }
    }

    Some(session)
}

/// Parses ISO-8601 UTC timestamps of the form `YYYY-MM-DDTHH:MM:SS[.fff]Z`
/// into `SystemTime`.
///
/// This is intentionally narrow: it covers the format produced by Kimi Code's
/// `state.json` without pulling in a full date-time library.
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
        std::env::temp_dir().join(format!("waitagent-test-kimi-{nanos}"))
    }

    fn cleanup(home: &Path) {
        let _ = fs::remove_dir_all(home);
    }

    fn write_fixture(home: &Path, entries: &[(&str, &str, &str, &str, &str)]) {
        fs::create_dir_all(home.join("sessions")).unwrap();
        let mut index = fs::File::create(home.join("session_index.jsonl")).unwrap();
        for (id, work_dir, title, last_prompt, updated_at) in entries {
            let session_dir = home.join("sessions").join(id);
            fs::create_dir_all(&session_dir).unwrap();
            let state = serde_json::json!({
                "title": title,
                "lastPrompt": last_prompt,
                "workDir": work_dir,
                "updatedAt": updated_at,
                "createdAt": "2026-07-13T09:20:00.000Z",
            });
            fs::write(session_dir.join("state.json"), state.to_string()).unwrap();
            writeln!(
                index,
                "{}",
                serde_json::json!({
                    "sessionId": id,
                    "sessionDir": session_dir.to_str().unwrap(),
                    "workDir": work_dir,
                })
            )
            .unwrap();
        }
    }

    #[test]
    fn list_sessions_reads_index_and_state() {
        let home = make_temp_home();
        write_fixture(
            &home,
            &[(
                "session_a",
                "/tmp/a",
                "Session A",
                "prompt a",
                "2026-07-13T09:21:28.024Z",
            )],
        );

        let provider = KimiSessionProvider::with_home(home.clone());
        let sessions = provider.list_sessions().unwrap();
        cleanup(&home);

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "session_a");
        assert_eq!(sessions[0].title.as_deref(), Some("Session A"));
        assert_eq!(sessions[0].cwd, Some(PathBuf::from("/tmp/a")));
        assert!(sessions[0].updated_at.is_some());
    }

    #[test]
    fn list_sessions_title_fallback_to_last_prompt() {
        let home = make_temp_home();
        let session_dir = home.join("sessions").join("session_b");
        fs::create_dir_all(&session_dir).unwrap();
        fs::write(
            session_dir.join("state.json"),
            serde_json::json!({
                "lastPrompt": "fallback prompt",
                "workDir": "/tmp/b",
                "updatedAt": "2026-07-13T10:00:00.000Z",
            })
            .to_string(),
        )
        .unwrap();
        let mut index = fs::File::create(home.join("session_index.jsonl")).unwrap();
        writeln!(
            index,
            "{}",
            serde_json::json!({
                "sessionId": "session_b",
                "sessionDir": session_dir.to_str().unwrap(),
                "workDir": "/tmp/b",
            })
        )
        .unwrap();

        let provider = KimiSessionProvider::with_home(home.clone());
        let sessions = provider.list_sessions().unwrap();
        cleanup(&home);

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].title.as_deref(), Some("fallback prompt"));
    }

    #[test]
    fn list_sessions_skips_malformed_lines() {
        let home = make_temp_home();
        let session_dir = home.join("sessions").join("session_c");
        fs::create_dir_all(&session_dir).unwrap();
        fs::write(
            session_dir.join("state.json"),
            serde_json::json!({
                "title": "Session C",
                "workDir": "/tmp/c",
                "updatedAt": "2026-07-13T11:00:00.000Z",
            })
            .to_string(),
        )
        .unwrap();
        let mut index = fs::File::create(home.join("session_index.jsonl")).unwrap();
        writeln!(index, "not json").unwrap();
        writeln!(
            index,
            "{}",
            serde_json::json!({
                "sessionId": "session_c",
                "sessionDir": session_dir.to_str().unwrap(),
                "workDir": "/tmp/c",
            })
        )
        .unwrap();

        let provider = KimiSessionProvider::with_home(home.clone());
        let sessions = provider.list_sessions().unwrap();
        cleanup(&home);

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "session_c");
    }

    #[test]
    fn list_sessions_sorts_by_updated_at_descending() {
        let home = make_temp_home();
        write_fixture(
            &home,
            &[
                (
                    "session_old",
                    "/tmp/old",
                    "Old",
                    "old",
                    "2026-07-10T00:00:00.000Z",
                ),
                (
                    "session_new",
                    "/tmp/new",
                    "New",
                    "new",
                    "2026-07-13T00:00:00.000Z",
                ),
            ],
        );

        let provider = KimiSessionProvider::with_home(home.clone());
        let sessions = provider.list_sessions().unwrap();
        cleanup(&home);

        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].id, "session_new");
        assert_eq!(sessions[1].id, "session_old");
    }

    #[test]
    fn resume_command_format() {
        let session = AgentSession {
            id: "session_x".into(),
            title: None,
            cwd: None,
            updated_at: None,
        };
        let provider = KimiSessionProvider::new();
        let cmd = provider.resume_command(&session);
        assert_eq!(cmd.program, "kimi");
        assert_eq!(cmd.args, vec!["--session", "session_x"]);
    }

    #[test]
    fn parse_iso8601_utc_round_trip() {
        let st = parse_iso8601_utc_to_system_time("2026-07-13T09:21:28.024Z").unwrap();
        let since = st.duration_since(SystemTime::UNIX_EPOCH).unwrap();
        // 2026-07-13 09:21:28 UTC
        assert_eq!(since.as_secs(), 1_783_934_488);
        assert_eq!(since.subsec_millis(), 24);
    }

    #[test]
    fn resolve_kimi_home_prefers_kimi_code_home() {
        // This test mutates the process environment. It is isolated from other
        // tests by not running in parallel with them, but Rust runs tests in
        // parallel by default. To avoid flakes, this test only validates the
        // precedence logic by temporarily setting and immediately restoring
        // variables while holding no locks that other tests need.
        let original_kimi_code_home = std::env::var_os("KIMI_CODE_HOME");
        let original_kimi_home = std::env::var_os("KIMI_HOME");

        std::env::set_var("KIMI_CODE_HOME", "/code/home");
        std::env::set_var("KIMI_HOME", "/kimi/home");
        let home = resolve_kimi_home().unwrap();

        assert_eq!(home, PathBuf::from("/code/home"));

        match original_kimi_code_home {
            Some(v) => std::env::set_var("KIMI_CODE_HOME", v),
            None => std::env::remove_var("KIMI_CODE_HOME"),
        }
        match original_kimi_home {
            Some(v) => std::env::set_var("KIMI_HOME", v),
            None => std::env::remove_var("KIMI_HOME"),
        }
    }
}
