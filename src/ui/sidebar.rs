use crate::domain::session_catalog::ManagedSessionRecord;

pub struct SidebarUi;

impl SidebarUi {
    pub fn render(
        active_socket: &str,
        active_session: &str,
        sessions: &[ManagedSessionRecord],
        width: usize,
        height: usize,
    ) -> String {
        let width = width.max(1);
        let height = height.max(1);
        let mut lines = Vec::new();
        lines.push(fit(" Sessions  [h] hide", width));
        if height > 1 {
            lines.push(fit(" ← back  ↑↓ move  enter switch", width));
        }
        if sessions.is_empty() {
            while lines.len() + 1 < height {
                lines.push(blank(width));
            }
            lines.push(fit(" (no sessions)", width));
            return lines.join("\n");
        }

        let selected = selected_session(active_socket, active_session, sessions)
            .unwrap_or_else(|| &sessions[0]);
        let detail_lines = selected_detail_lines(selected, width);
        let session_capacity = height.saturating_sub(lines.len() + detail_lines.len());

        for session in sessions.iter().take(session_capacity) {
            lines.push(session_row(
                session,
                session.address.server_id() == active_socket
                    && session.address.session_id() == active_session,
                width,
            ));
        }

        while lines.len() + detail_lines.len() < height {
            lines.push(blank(width));
        }

        lines.extend(detail_lines);
        lines.join("\n")
    }
}

fn selected_session<'a>(
    active_socket: &str,
    active_session: &str,
    sessions: &'a [ManagedSessionRecord],
) -> Option<&'a ManagedSessionRecord> {
    sessions.iter().find(|session| {
        session.address.server_id() == active_socket
            && session.address.session_id() == active_session
    })
}

fn session_row(session: &ManagedSessionRecord, active: bool, width: usize) -> String {
    let marker = if active { ">" } else { " " };
    let state = session.task_state.label();
    let reserved = marker.len() + 1 + 1 + state.len();
    let label_width = width.saturating_sub(reserved);
    let label = pad_right(
        &truncate_left(&session.display_label(), label_width),
        label_width,
    );
    format!("{marker} {label} {state}")
}

fn selected_detail_lines(session: &ManagedSessionRecord, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    if width == 0 {
        return lines;
    }
    lines.push(fit(&"─".repeat(width), width));
    lines.push(fit(
        &format!(
            " {} | {}",
            session.display_label(),
            session.task_state.label()
        ),
        width,
    ));
    let current_path = session
        .current_path
        .as_ref()
        .or(session.workspace_dir.as_ref())
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "-".to_string());
    lines.push(fit(&format!(" dir: {current_path}"), width));
    lines.push(fit(
        &format!(
            " clients:{} windows:{}",
            session.attached_clients, session.window_count
        ),
        width,
    ));
    lines
}

fn fit(line: &str, width: usize) -> String {
    pad_right(&truncate_left(line, width), width)
}

fn blank(width: usize) -> String {
    " ".repeat(width)
}

fn pad_right(text: &str, width: usize) -> String {
    format!("{text:<width$}")
}

fn truncate_left(text: &str, width: usize) -> String {
    text.chars().take(width).collect()
}

#[cfg(test)]
mod tests {
    use super::SidebarUi;
    use crate::domain::session_catalog::{
        ManagedSessionAddress, ManagedSessionRecord, ManagedSessionTaskState,
    };
    use std::path::PathBuf;

    #[test]
    fn sidebar_ui_renders_legacy_style_header_rows_and_detail_footer() {
        let output = SidebarUi::render(
            "wa-1",
            "waitagent-1",
            &[ManagedSessionRecord {
                address: ManagedSessionAddress::local_tmux("wa-1", "waitagent-1"),
                workspace_dir: Some(PathBuf::from("/tmp/demo")),
                workspace_key: Some("1234".to_string()),
                attached_clients: 2,
                window_count: 1,
                command_name: Some("codex".to_string()),
                current_path: Some(PathBuf::from("/tmp/demo")),
                task_state: ManagedSessionTaskState::Confirm,
            }],
            28,
            8,
        );

        assert!(output.contains(" Sessions  [h] hide"));
        assert!(output.contains("← back  ↑↓ move  enter swit"));
        assert!(output.contains("> codex@local"));
        assert!(output.contains("CONFIRM"));
        assert!(output.contains(" dir: /tmp/demo"));
        assert!(output.contains(" clients:2 windows:1"));
    }
}
