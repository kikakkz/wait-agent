use crate::domain::session_catalog::{ManagedSessionRecord, ManagedSessionTaskState};

pub struct FooterUi;

impl FooterUi {
    pub fn render(
        active_socket: &str,
        active_session: &str,
        sessions: &[ManagedSessionRecord],
        width: usize,
    ) -> String {
        let width = width.max(1);
        let active = active_session_record(active_socket, active_session, sessions);
        let counts = task_counts(sessions);
        let left = format!(
            "WaitAgent | [c] create  [s] sessions  [Enter] switch | total:{} R:{} I:{} C:{} U:{}",
            sessions.len(),
            counts.running,
            counts.input,
            counts.confirm,
            counts.unknown
        );
        let right = active
            .and_then(|session| {
                session
                    .current_path
                    .as_ref()
                    .or(session.workspace_dir.as_ref())
            })
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| active_session.to_string());
        join_left_right(&left, &right, width)
    }
}

fn active_session_record<'a>(
    active_socket: &str,
    active_session: &str,
    sessions: &'a [ManagedSessionRecord],
) -> Option<&'a ManagedSessionRecord> {
    sessions.iter().find(|session| {
        session.address.server_id() == active_socket
            && session.address.session_id() == active_session
    })
}

#[derive(Default)]
struct TaskCounts {
    running: usize,
    input: usize,
    confirm: usize,
    unknown: usize,
}

fn task_counts(sessions: &[ManagedSessionRecord]) -> TaskCounts {
    let mut counts = TaskCounts::default();
    for session in sessions {
        match session.task_state {
            ManagedSessionTaskState::Running => counts.running += 1,
            ManagedSessionTaskState::Input => counts.input += 1,
            ManagedSessionTaskState::Confirm => counts.confirm += 1,
            ManagedSessionTaskState::Unknown => counts.unknown += 1,
        }
    }
    counts
}

fn join_left_right(left: &str, right: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let right_width = right.chars().count().min(width.saturating_div(2).max(16));
    let right = truncate_path_from_left(right, right_width.min(width));
    let right_len = right.chars().count();
    if right_len >= width {
        return right;
    }

    let spacer = 1;
    let left_width = width.saturating_sub(right_len + spacer);
    let left = truncate_from_right(left, left_width);
    format!("{left:<left_width$} {right}")
}

fn truncate_from_right(value: &str, width: usize) -> String {
    value.chars().take(width).collect()
}

fn truncate_path_from_left(value: &str, width: usize) -> String {
    let chars = value.chars().collect::<Vec<_>>();
    if chars.len() <= width {
        return value.to_string();
    }
    if width <= 1 {
        return chars.into_iter().take(width).collect();
    }
    let tail = chars[chars.len().saturating_sub(width - 1)..]
        .iter()
        .collect::<String>();
    format!("…{tail}")
}

#[cfg(test)]
mod tests {
    use super::FooterUi;
    use crate::domain::session_catalog::{
        ManagedSessionAddress, ManagedSessionRecord, ManagedSessionTaskState,
    };
    use std::path::PathBuf;

    #[test]
    fn footer_ui_renders_menu_counts_and_right_aligned_directory() {
        let output = FooterUi::render(
            "wa-1",
            "waitagent-1",
            &[ManagedSessionRecord {
                address: ManagedSessionAddress::local_tmux("wa-1", "waitagent-1"),
                workspace_dir: Some(PathBuf::from("/tmp/demo")),
                workspace_key: None,
                attached_clients: 1,
                window_count: 1,
                command_name: Some("codex".to_string()),
                current_path: Some(PathBuf::from("/tmp/demo")),
                task_state: ManagedSessionTaskState::Input,
            }],
            96,
        );

        assert!(output.contains("WaitAgent | [c] create  [s] sessions  [Enter] switch"));
        assert!(output.contains("total:1 R:0 I:1 C:0 U:0"));
        assert!(output.ends_with("/tmp/demo"));
        assert!(!output.contains('\n'));
    }
}
