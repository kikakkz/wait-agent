#![allow(dead_code)]

use crate::terminal::ScreenState;
use std::collections::HashMap;
use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionAddress {
    node_id: String,
    session_id: String,
}

impl SessionAddress {
    pub fn new(node_id: impl Into<String>, session_id: impl Into<String>) -> Self {
        Self {
            node_id: node_id.into(),
            session_id: session_id.into(),
        }
    }

    pub fn node_id(&self) -> &str {
        &self.node_id
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }
}

impl fmt::Display for SessionAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.node_id, self.session_id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionStatus {
    Starting,
    Running,
    WaitingInput,
    Idle,
    Exited,
}

#[derive(Debug, Clone)]
pub struct SessionRecord {
    address: SessionAddress,
    pub title: String,
    pub command_line: String,
    pub current_working_dir: Option<String>,
    pub status: SessionStatus,
    pub process_id: Option<u32>,
    pub screen_state: Option<ScreenState>,
    pub snapshot_version: u64,
    pub created_at_unix_ms: u128,
    pub last_output_at_unix_ms: Option<u128>,
    pub last_input_at_unix_ms: Option<u128>,
}

impl SessionRecord {
    pub fn address(&self) -> &SessionAddress {
        &self.address
    }
}

#[derive(Debug, Default)]
pub struct SessionRegistry {
    sessions: HashMap<SessionAddress, SessionRecord>,
    local_counter: u64,
}

impl SessionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn create_local_session(
        &mut self,
        node_id: String,
        title: String,
        command_line: String,
    ) -> SessionRecord {
        self.local_counter += 1;
        let session_id = format!("session-{}", self.local_counter);
        let address = SessionAddress::new(node_id, session_id);
        let record = SessionRecord {
            address: address.clone(),
            title,
            command_line,
            current_working_dir: None,
            status: SessionStatus::Starting,
            process_id: None,
            screen_state: None,
            snapshot_version: 0,
            created_at_unix_ms: now_unix_ms(),
            last_output_at_unix_ms: None,
            last_input_at_unix_ms: None,
        };

        self.sessions.insert(address, record.clone());
        record
    }

    pub fn get(&self, address: &SessionAddress) -> Option<&SessionRecord> {
        self.sessions.get(address)
    }

    pub fn mark_running(
        &mut self,
        address: &SessionAddress,
        process_id: Option<u32>,
    ) -> Option<&SessionRecord> {
        self.mark_running_at(address, process_id, None)
    }

    pub(crate) fn mark_running_at(
        &mut self,
        address: &SessionAddress,
        process_id: Option<u32>,
        output_at_unix_ms: Option<u128>,
    ) -> Option<&SessionRecord> {
        let record = self.sessions.get_mut(address)?;
        record.status = SessionStatus::Running;
        record.process_id = process_id;
        record.last_output_at_unix_ms = output_at_unix_ms;
        Some(record)
    }

    pub fn mark_exited(&mut self, address: &SessionAddress) -> Option<&SessionRecord> {
        let record = self.sessions.get_mut(address)?;
        record.status = SessionStatus::Exited;
        Some(record)
    }

    pub fn mark_output(&mut self, address: &SessionAddress) -> Option<&SessionRecord> {
        self.mark_output_at(address, now_unix_ms())
    }

    pub(crate) fn mark_output_at(
        &mut self,
        address: &SessionAddress,
        at_unix_ms: u128,
    ) -> Option<&SessionRecord> {
        let record = self.sessions.get_mut(address)?;
        record.status = SessionStatus::Running;
        record.last_output_at_unix_ms = Some(at_unix_ms);
        Some(record)
    }

    pub fn mark_input(&mut self, address: &SessionAddress) -> Option<&SessionRecord> {
        self.mark_input_at(address, now_unix_ms())
    }

    pub(crate) fn mark_input_at(
        &mut self,
        address: &SessionAddress,
        at_unix_ms: u128,
    ) -> Option<&SessionRecord> {
        let record = self.sessions.get_mut(address)?;
        record.status = SessionStatus::Running;
        record.last_input_at_unix_ms = Some(at_unix_ms);
        Some(record)
    }

    pub fn set_title(
        &mut self,
        address: &SessionAddress,
        title: impl Into<String>,
    ) -> Option<&SessionRecord> {
        let record = self.sessions.get_mut(address)?;
        record.title = title.into();
        Some(record)
    }

    pub fn update_screen_state(
        &mut self,
        address: &SessionAddress,
        screen_state: ScreenState,
    ) -> Option<&SessionRecord> {
        let record = self.sessions.get_mut(address)?;
        if let Some(working_dir) = screen_state
            .active_snapshot()
            .window_title
            .as_deref()
            .and_then(extract_working_dir_from_title)
        {
            record.current_working_dir = Some(working_dir.to_string());
        }
        record.screen_state = Some(screen_state);
        record.snapshot_version += 1;
        Some(record)
    }

    pub fn list(&self) -> Vec<&SessionRecord> {
        let mut sessions = self.sessions.values().collect::<Vec<_>>();
        sessions.sort_by(|left, right| {
            left.created_at_unix_ms
                .cmp(&right.created_at_unix_ms)
                .then_with(|| left.address.session_id().cmp(right.address.session_id()))
        });
        sessions
    }
}

fn now_unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn extract_working_dir_from_title(title: &str) -> Option<&str> {
    let trimmed = title.trim();
    if looks_like_working_dir(trimmed) {
        return Some(trimmed);
    }

    if let Some((_, tail)) = trimmed.rsplit_once(": ") {
        let tail = tail.trim();
        if looks_like_working_dir(tail) {
            return Some(tail);
        }
    }

    trimmed
        .split_whitespace()
        .rev()
        .find(|segment| looks_like_working_dir(segment))
}

fn looks_like_working_dir(value: &str) -> bool {
    matches!(value.chars().next(), Some('/' | '~' | '.'))
}

#[cfg(test)]
mod tests {
    use super::{extract_working_dir_from_title, SessionRegistry};
    use crate::terminal::{TerminalEngine, TerminalSize};

    #[test]
    fn creates_sessions_with_stable_local_addresses() {
        let mut registry = SessionRegistry::new();
        let session = registry.create_local_session(
            "devbox-1".to_string(),
            "claude".to_string(),
            "claude".to_string(),
        );

        assert_eq!(session.address().node_id(), "devbox-1");
        assert_eq!(session.address().session_id(), "session-1");
        assert_eq!(session.title, "claude");
        assert!(session.current_working_dir.is_none());
        assert!(session.process_id.is_none());
        assert!(session.screen_state.is_none());
        assert_eq!(session.snapshot_version, 0);
    }

    #[test]
    fn updates_screen_state_and_bumps_snapshot_version() {
        let mut registry = SessionRegistry::new();
        let session = registry.create_local_session(
            "devbox-1".to_string(),
            "claude".to_string(),
            "claude".to_string(),
        );
        let mut engine = TerminalEngine::new(TerminalSize {
            rows: 2,
            cols: 6,
            pixel_width: 0,
            pixel_height: 0,
        });
        engine.feed(b"\x1b]0;k@k: /opt/data/workspace/wait-agent\x07");
        engine.feed(b"hello");

        let updated = registry
            .update_screen_state(session.address(), engine.state())
            .expect("session should update");

        assert_eq!(updated.snapshot_version, 1);
        assert_eq!(
            updated
                .screen_state
                .as_ref()
                .expect("screen state should exist")
                .normal
                .lines[0],
            "hello "
        );
        assert_eq!(
            updated.current_working_dir.as_deref(),
            Some("/opt/data/workspace/wait-agent")
        );
    }

    #[test]
    fn extracts_working_dir_from_common_shell_titles() {
        assert_eq!(
            extract_working_dir_from_title("k@k: /opt/data/workspace/wait-agent"),
            Some("/opt/data/workspace/wait-agent")
        );
        assert_eq!(
            extract_working_dir_from_title("/tmp/project"),
            Some("/tmp/project")
        );
        assert_eq!(
            extract_working_dir_from_title("devbox ~/project"),
            Some("~/project")
        );
        assert_eq!(extract_working_dir_from_title("codex"), None);
    }

    #[test]
    fn updates_title_without_touching_command_line() {
        let mut registry = SessionRegistry::new();
        let session = registry.create_local_session(
            "devbox-1".to_string(),
            "bash".to_string(),
            "/bin/bash".to_string(),
        );

        let updated = registry
            .set_title(session.address(), "codex")
            .expect("session should update");

        assert_eq!(updated.title, "codex");
        assert_eq!(updated.command_line, "/bin/bash");
    }
}
