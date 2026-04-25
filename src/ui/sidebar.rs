use crate::domain::chrome::SidebarViewModel;
use crate::domain::session_catalog::{ManagedSessionRecord, ManagedSessionTaskState};
use crate::ui::chrome::{
    right_align, sidebar_row_prefix, style_sidebar_badge, style_sidebar_detail_line,
    style_sidebar_header_line, style_sidebar_hint_line, style_sidebar_item_line,
    style_sidebar_separator_line, SidebarBadgeState, SidebarRowStyle,
};

pub struct SidebarUi;

impl SidebarUi {
    pub fn render_view_model(model: &SidebarViewModel, now_millis: u128) -> String {
        Self::render(
            &model.active_socket,
            &model.active_session,
            model.active_target.as_deref(),
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
        active_target: Option<&str>,
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
        push_line(
            &mut lines,
            style_sidebar_header_line(" Sessions  [h] hide", width),
            height,
        );
        push_line(&mut lines, render_separator_row(width), height);
        if lines.len() == height {
            return lines.join("\n");
        }

        if sessions.is_empty() {
            while lines.len() + 2 < height {
                lines.push(style_sidebar_item_line("", width, SidebarRowStyle::Normal));
            }
            push_line(&mut lines, render_separator_row(width), height);
            lines.push(style_sidebar_detail_line(
                &right_align("(no sessions)", width),
                width,
            ));
            return lines.join("\n");
        }

        let selected = selected_session(
            active_socket,
            active_session,
            active_target,
            selected_target,
            sessions,
        )
        .unwrap_or(&sessions[0]);
        let detail_line = selected_detail_line(selected, width);
        let session_capacity = height.saturating_sub(lines.len() + 2);
        for session in sessions.iter().take(session_capacity) {
            let qualified_target = session.address.qualified_target();
            let is_current = active_target == Some(qualified_target.as_str());
            let is_selected = qualified_target == selected.address.qualified_target();
            lines.push(render_session_row(
                session,
                is_current,
                is_selected,
                width,
                _now_millis,
            ));
        }

        while lines.len() + 2 < height {
            lines.push(style_sidebar_item_line("", width, SidebarRowStyle::Normal));
        }

        push_line(&mut lines, render_separator_row(width), height);
        lines.push(detail_line);
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
    active_target: Option<&str>,
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
            active_target.and_then(|target| {
                sessions
                    .iter()
                    .find(|session| session.address.qualified_target() == target)
            })
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
    now_millis: u128,
) -> String {
    let marker = if is_selected {
        ">"
    } else if is_current {
        "*"
    } else {
        " "
    };
    let row_style = if is_selected {
        SidebarRowStyle::Selected
    } else if is_current {
        SidebarRowStyle::Current
    } else {
        SidebarRowStyle::Normal
    };
    let badge_state = sidebar_badge_state(session.task_state);
    let (badge, badge_width) = style_sidebar_badge(badge_state, row_style, now_millis);
    let reserved = marker.len() + 1 + 1 + badge_width;
    let label_width = width.saturating_sub(reserved);
    let label = pad_right(
        &truncate_left(&session.display_label(), label_width),
        label_width,
    );
    let prefix = sidebar_row_prefix(row_style);
    format!("{prefix}{marker} {label} {badge}\x1b[0m")
}

fn selected_detail_line(session: &ManagedSessionRecord, width: usize) -> String {
    let detail = format!("{} {}", session.display_label(), session.task_state.label());
    style_sidebar_detail_line(&right_align(&detail, width), width)
}

fn render_separator_row(width: usize) -> String {
    style_sidebar_separator_line(&"─".repeat(width), width)
}

fn sidebar_badge_state(state: ManagedSessionTaskState) -> SidebarBadgeState {
    match state {
        ManagedSessionTaskState::Running => SidebarBadgeState::Running,
        ManagedSessionTaskState::Input => SidebarBadgeState::Input,
        ManagedSessionTaskState::Confirm => SidebarBadgeState::Confirm,
        ManagedSessionTaskState::Unknown => SidebarBadgeState::Unknown,
    }
}

fn push_line(lines: &mut Vec<String>, line: String, height: usize) {
    if lines.len() < height {
        lines.push(line);
    }
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
    fn sidebar_ui_renders_items_with_badges_and_single_detail_footer() {
        let output = SidebarUi::render(
            "wa-1",
            "waitagent-1",
            Some("wa-1:waitagent-1"),
            Some("wa-2:waitagent-2"),
            &[
                ManagedSessionRecord {
                    address: ManagedSessionAddress::local_tmux("wa-1", "waitagent-1"),
                    workspace_dir: Some(PathBuf::from("/tmp/demo")),
                    workspace_key: Some("1234".to_string()),
                    session_role: None,
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
                    session_role: None,
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
        assert!(output.contains("────"));
        assert!(output.contains("* bash@local"));
        assert!(output.contains("> codex@local"));
        assert!(output.contains("\u{1b}[38;5;227m🔊I"));
        assert!(output.contains("\u{1b}[38;5;215m📢C"));
        assert!(output.contains("codex@local CONFIRM"));
        assert!(!output.contains("----------------"));
        assert!(!output.contains(" id: 2"));
        assert!(output.contains("\u{1b}[48;5;240m"));
    }

    #[test]
    fn sidebar_ui_renders_collapsed_marker_for_hidden_width() {
        let output = SidebarUi::render("wa-1", "waitagent-1", None, None, &[], 1, 3, 0);

        assert!(output.contains("<"));
    }
}
