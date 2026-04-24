use crate::domain::chrome::FooterViewModel;
use crate::domain::session_catalog::{ManagedSessionRecord, ManagedSessionTaskState};
use crate::ui::chrome::style_status_line;

pub struct FooterUi;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FooterProjection {
    Pane,
    FullscreenStatus,
}

impl FooterUi {
    pub fn render_view_model(model: &FooterViewModel) -> String {
        if model.fullscreen {
            Self::render_fullscreen(
                &model.active_socket,
                &model.active_session,
                &model.sessions,
                model.width,
            )
        } else {
            Self::render(
                &model.active_socket,
                &model.active_session,
                &model.sessions,
                model.width,
            )
        }
    }

    pub fn render(
        active_socket: &str,
        active_session: &str,
        sessions: &[ManagedSessionRecord],
        width: usize,
    ) -> String {
        render_footer(
            active_socket,
            active_session,
            sessions,
            width,
            FooterProjection::Pane,
        )
    }

    pub fn render_fullscreen(
        active_socket: &str,
        active_session: &str,
        sessions: &[ManagedSessionRecord],
        width: usize,
    ) -> String {
        render_footer(
            active_socket,
            active_session,
            sessions,
            width,
            FooterProjection::FullscreenStatus,
        )
    }
}

fn render_footer(
    active_socket: &str,
    active_session: &str,
    sessions: &[ManagedSessionRecord],
    width: usize,
    projection: FooterProjection,
) -> String {
    let width = width.max(1);
    let active = active_session_record(active_socket, active_session, sessions);
    let counts = task_counts(sessions);
    let left = match projection {
        FooterProjection::Pane => {
            "keys: ^W cmd  ^B/^F switch  ^N new  ^O full on/off  ^L picker  ^X close  ^Q quit"
                .to_string()
        }
        FooterProjection::FullscreenStatus => format!(
            "keys: [Ctrl-b c] new  [Ctrl-b s] sessions  [Ctrl-o] full off | total:{} R:{} I:{} C:{} U:{}",
            sessions.len(),
            counts.running,
            counts.input,
            counts.confirm,
            counts.unknown
        ),
    };
    let right = active
        .and_then(|session| {
            session
                .current_path
                .as_ref()
                .or(session.workspace_dir.as_ref())
        })
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| active_session.to_string());
    let line = join_left_right(&left, &right, width);
    match projection {
        FooterProjection::Pane => style_status_line(&line, width),
        FooterProjection::FullscreenStatus => line,
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

        assert!(output.contains("keys: ^W cmd"));
        assert!(output.contains("^N new"));
        assert!(output.contains("^Q quit"));
        assert!(output.contains("/tmp/demo"));
        assert!(!output.contains('\n'));
        assert!(output.starts_with("\u{1b}[48;5;24m"));
    }

    #[test]
    fn fullscreen_footer_ui_uses_prefixed_menu_hints() {
        let output = FooterUi::render_fullscreen(
            "wa-1",
            "waitagent-1",
            &[ManagedSessionRecord {
                address: ManagedSessionAddress::local_tmux("wa-1", "waitagent-1"),
                workspace_dir: Some(PathBuf::from("/tmp/demo")),
                workspace_key: None,
                attached_clients: 1,
                window_count: 1,
                command_name: Some("bash".to_string()),
                current_path: Some(PathBuf::from("/tmp/demo")),
                task_state: ManagedSessionTaskState::Input,
            }],
            120,
        );

        assert!(output.contains("[Ctrl-b c] new"));
        assert!(output.contains("[Ctrl-b s] sessions"));
        assert!(output.contains("[Ctrl-o] full off"));
        assert!(output.contains("total:1 R:0 I:1 C:0 U:0"));
        assert!(output.contains("/tmp/demo"));
    }
}
