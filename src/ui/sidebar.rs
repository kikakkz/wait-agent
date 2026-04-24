use crate::domain::chrome::SidebarViewModel;
use crate::domain::session_catalog::ManagedSessionRecord;
use crate::ui::chrome::{
    style_sidebar_detail_line, style_sidebar_header_line, style_sidebar_hint_line,
    style_sidebar_item_line, SidebarRowStyle,
};

pub struct SidebarUi;

impl SidebarUi {
    pub fn render_view_model(model: &SidebarViewModel, now_millis: u128) -> String {
        Self::render(
            &model.active_socket,
            &model.active_session,
            model.selected_target.as_deref(),
            &model.sessions,
            model.surface.width,
            model.surface.height,
            now_millis,
        )
    }

    pub fn render(
        active_socket: &str,
        active_session: &str,
        selected_target: Option<&str>,
        sessions: &[ManagedSessionRecord],
        width: usize,
        height: usize,
        _now_millis: u128,
    ) -> String {
        let width = width.max(1);
        let height = height.max(1);
        if width <= 2 {
            return render_collapsed(width, height);
        }

        let mut lines = Vec::new();
        lines.push(style_sidebar_header_line(" Sessions  [h] hide", width));
        if height > 1 {
            lines.push(style_sidebar_hint_line(
                " Left back  Up/Down  Enter switch",
                width,
            ));
        }

        if sessions.is_empty() {
            while lines.len() + 1 < height {
                lines.push(style_sidebar_item_line("", width, SidebarRowStyle::Normal));
            }
            lines.push(style_sidebar_detail_line(" (no sessions)", width));
            return lines.join("\n");
        }

        let selected = selected_session(active_socket, active_session, selected_target, sessions)
            .unwrap_or(&sessions[0]);
        let detail_lines = selected_detail_lines(selected, width);
        let session_capacity = height.saturating_sub(lines.len() + detail_lines.len());
        for session in sessions.iter().take(session_capacity) {
            let is_current = session.address.server_id() == active_socket
                && session.address.session_id() == active_session;
            let is_selected =
                session.address.qualified_target() == selected.address.qualified_target();
            lines.push(render_session_row(session, is_current, is_selected, width));
        }

        while lines.len() + detail_lines.len() < height {
            lines.push(style_sidebar_item_line("", width, SidebarRowStyle::Normal));
        }

        lines.extend(detail_lines);
        lines.join("\n")
    }
}

fn render_collapsed(width: usize, height: usize) -> String {
    let mut lines = vec![style_sidebar_hint_line("<", width)];
    while lines.len() < height {
        lines.push(style_sidebar_item_line("", width, SidebarRowStyle::Normal));
    }
    lines.join("\n")
}

fn selected_session<'a>(
    active_socket: &str,
    active_session: &str,
    selected_target: Option<&str>,
    sessions: &'a [ManagedSessionRecord],
) -> Option<&'a ManagedSessionRecord> {
    selected_target
        .and_then(|target| {
            sessions
                .iter()
                .find(|session| session.address.qualified_target() == target)
        })
        .or_else(|| {
            sessions.iter().find(|session| {
                session.address.server_id() == active_socket
                    && session.address.session_id() == active_session
            })
        })
}

fn render_session_row(
    session: &ManagedSessionRecord,
    is_current: bool,
    is_selected: bool,
    width: usize,
) -> String {
    let marker = if is_selected {
        ">"
    } else if is_current {
        "*"
    } else {
        " "
    };
    let state = session.task_state.label();
    let reserved = marker.len() + 1 + 1 + state.len();
    let label_width = width.saturating_sub(reserved);
    let label = pad_right(
        &truncate_left(&session.display_label(), label_width),
        label_width,
    );
    let row_style = if is_selected {
        SidebarRowStyle::Selected
    } else if is_current {
        SidebarRowStyle::Current
    } else {
        SidebarRowStyle::Normal
    };
    style_sidebar_item_line(&format!("{marker} {label} {state}"), width, row_style)
}

fn selected_detail_lines(session: &ManagedSessionRecord, width: usize) -> Vec<String> {
    if width == 0 {
        return Vec::new();
    }

    let current_path = session
        .current_path
        .as_ref()
        .or(session.workspace_dir.as_ref())
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "-".to_string());

    vec![
        style_sidebar_detail_line(&"-".repeat(width), width),
        style_sidebar_detail_line(
            &format!(
                " {} | {}",
                session.display_label(),
                session.task_state.label()
            ),
            width,
        ),
        style_sidebar_detail_line(
            &format!(" id: {}", session.address.display_session_id()),
            width,
        ),
        style_sidebar_detail_line(&format!(" dir: {current_path}"), width),
        style_sidebar_detail_line(
            &format!(
                " clients:{} windows:{}",
                session.attached_clients, session.window_count
            ),
            width,
        ),
    ]
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
    fn sidebar_ui_renders_plain_session_rows_and_detail_footer() {
        let output = SidebarUi::render(
            "wa-1",
            "waitagent-1",
            Some("wa-2:waitagent-2"),
            &[
                ManagedSessionRecord {
                    address: ManagedSessionAddress::local_tmux("wa-1", "waitagent-1"),
                    workspace_dir: Some(PathBuf::from("/tmp/demo")),
                    workspace_key: Some("1234".to_string()),
                    attached_clients: 2,
                    window_count: 1,
                    command_name: Some("bash".to_string()),
                    current_path: Some(PathBuf::from("/tmp/demo")),
                    task_state: ManagedSessionTaskState::Input,
                },
                ManagedSessionRecord {
                    address: ManagedSessionAddress::local_tmux("wa-2", "waitagent-2"),
                    workspace_dir: Some(PathBuf::from("/tmp/demo")),
                    workspace_key: Some("5678".to_string()),
                    attached_clients: 1,
                    window_count: 1,
                    command_name: Some("codex".to_string()),
                    current_path: Some(PathBuf::from("/tmp/demo")),
                    task_state: ManagedSessionTaskState::Confirm,
                },
            ],
            28,
            9,
            0,
        );

        assert!(output.starts_with("\u{1b}[48;5;236m"));
        assert!(output.contains(" Sessions  [h] hide"));
        assert!(output.contains("Up/Down"));
        assert!(output.contains("* bash@local"));
        assert!(output.contains("> codex@local"));
        assert!(output.contains("INPUT"));
        assert!(output.contains("CONFIRM"));
        assert!(output.contains("codex@local | CONFIRM"));
        assert!(output.contains(" id: 2"));
        assert!(output.contains("\u{1b}[48;5;240m"));
    }

    #[test]
    fn sidebar_ui_renders_collapsed_marker_for_hidden_width() {
        let output = SidebarUi::render("wa-1", "waitagent-1", None, &[], 1, 3, 0);

        assert!(output.contains("<"));
    }
}
