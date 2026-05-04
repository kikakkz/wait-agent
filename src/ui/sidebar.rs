use crate::domain::chrome::SidebarViewModel;
use crate::domain::session_catalog::{
    ManagedSessionRecord, ManagedSessionTaskState, SessionTransport,
};
use crate::ui::chrome::{
    display_width, right_align, sidebar_row_prefix, style_sidebar_badge, style_sidebar_detail_line,
    style_sidebar_header_line, style_sidebar_hint_line, style_sidebar_item_line,
    style_sidebar_separator_line, truncate_display_width, SidebarBadgeState, SidebarRowStyle,
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
        for session in sessions.iter().take(height.saturating_sub(lines.len() + 2)) {
            let qualified_target = session.address.qualified_target();
            let is_current = active_target == Some(qualified_target.as_str());
            let is_selected = qualified_target == selected.address.qualified_target();
            push_line(
                &mut lines,
                render_session_row(session, is_current, is_selected, width, _now_millis),
                height,
            );
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
    let primary_label = session_row_primary_label(session, label_width);
    let prefix = sidebar_row_prefix(row_style);
    format!("{prefix}{marker} {primary_label} {badge}\x1b[0m")
}

fn selected_detail_line(session: &ManagedSessionRecord, width: usize) -> String {
    let detail = selected_detail_text(session, width);
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
    let text = truncate_display_width(text, width);
    let padding = width.saturating_sub(display_width(&text));
    format!("{text}{}", " ".repeat(padding))
}

fn session_row_primary_label(session: &ManagedSessionRecord, width: usize) -> String {
    let full_label = session.display_label();
    if display_width(&full_label) <= width {
        return pad_right(&full_label, width);
    }

    if session.address.transport() == &SessionTransport::RemotePeer {
        let command_host_port = remote_command_host_port_label(session);
        if display_width(&command_host_port) <= width {
            return pad_right(&command_host_port, width);
        }
        let command_host = format!(
            "{}@{}",
            session.command_name.as_deref().unwrap_or("bash"),
            session.address.display_authority_id()
        );
        if display_width(&command_host) <= width {
            return pad_right(&command_host, width);
        }
        return pad_right(&truncate_display_width(&command_host_port, width), width);
    }

    pad_right(&truncate_display_width(&full_label, width), width)
}

fn remote_command_host_port_label(session: &ManagedSessionRecord) -> String {
    let authority = session.address.authority_id();
    let (host, port) = authority
        .split_once('#')
        .map(|(host, port)| (host, Some(port)))
        .unwrap_or((session.address.display_authority_id(), None));
    match port {
        Some(port) => format!(
            "{}@{}:{}",
            session.command_name.as_deref().unwrap_or("bash"),
            host,
            port
        ),
        None => format!(
            "{}@{}",
            session.command_name.as_deref().unwrap_or("bash"),
            host
        ),
    }
}

fn selected_detail_text(session: &ManagedSessionRecord, width: usize) -> String {
    let suffix =
        if session.availability != crate::domain::session_catalog::SessionAvailability::Online {
            session.availability.as_str().to_ascii_uppercase()
        } else {
            session.task_state.label().to_string()
        };
    let full_label = session.display_label();
    let full_detail = format!("{full_label} {suffix}");
    if display_width(&full_detail) <= width {
        return full_detail;
    }

    if session.address.transport() == &SessionTransport::RemotePeer {
        let command_host_label = format!(
            "{}@{}",
            session.command_name.as_deref().unwrap_or("bash"),
            session.address.display_authority_id()
        );
        let command_host_detail = format!("{command_host_label} {suffix}");
        if display_width(&command_host_detail) <= width {
            return command_host_detail;
        }

        let host_only_detail = format!("{} {suffix}", session.address.display_authority_id());
        if display_width(&host_only_detail) <= width {
            return host_only_detail;
        }

        let authority_only_label = session.address.display_authority_id();
        if display_width(authority_only_label) <= width {
            return authority_only_label.to_string();
        }
    }

    truncate_display_width(&full_detail, width)
}

#[cfg(test)]
mod tests {
    use super::SidebarUi;
    use crate::domain::session_catalog::{
        ManagedSessionAddress, ManagedSessionRecord, ManagedSessionTaskState, SessionAvailability,
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
                    selector: Some("wa-1:waitagent-1".to_string()),
                    availability: crate::domain::session_catalog::SessionAvailability::Online,
                    workspace_dir: Some(PathBuf::from("/tmp/demo")),
                    workspace_key: Some("1234".to_string()),
                    session_role: None,
                    opened_by: Vec::new(),
                    attached_clients: 2,
                    window_count: 1,
                    command_name: Some("bash".to_string()),
                    current_path: Some(PathBuf::from("/tmp/demo")),
                    task_state: ManagedSessionTaskState::Input,
                },
                ManagedSessionRecord {
                    address: ManagedSessionAddress::local_tmux("wa-2", "waitagent-2"),
                    selector: Some("wa-2:waitagent-2".to_string()),
                    availability: crate::domain::session_catalog::SessionAvailability::Online,
                    workspace_dir: Some(PathBuf::from("/tmp/demo")),
                    workspace_key: Some("5678".to_string()),
                    session_role: None,
                    opened_by: Vec::new(),
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

    #[test]
    fn sidebar_ui_renders_remote_session_on_single_line_with_host_port() {
        let output = SidebarUi::render(
            "wa-1",
            "waitagent-1",
            None,
            Some("192.168.31.182#7513:372645a93b9cd222"),
            &[ManagedSessionRecord {
                address: ManagedSessionAddress::remote_peer(
                    "192.168.31.182#7513",
                    "372645a93b9cd222",
                ),
                selector: Some("192.168.31.182#7513:372645a93b9cd222".to_string()),
                availability: SessionAvailability::Online,
                workspace_dir: Some(PathBuf::from("/home/kk/wait-agent")),
                workspace_key: Some("372645a93b9cd222".to_string()),
                session_role: None,
                opened_by: Vec::new(),
                attached_clients: 0,
                window_count: 1,
                command_name: Some("bash".to_string()),
                current_path: Some(PathBuf::from("/home/kk/wait-agent")),
                task_state: ManagedSessionTaskState::Input,
            }],
            32,
            6,
            0,
        );

        assert!(output.contains("bash@192.168.31.182:7513"));
        assert!(output.contains("192.168.31.182"));
        assert!(!output.contains("session 372645a93b9cd222"));
    }

    #[test]
    fn sidebar_ui_preserves_remote_host_in_detail_line_when_width_is_tight() {
        let output = SidebarUi::render(
            "wa-1",
            "waitagent-1",
            None,
            Some("192.168.31.182#7513:372645a93b9cd222"),
            &[ManagedSessionRecord {
                address: ManagedSessionAddress::remote_peer(
                    "192.168.31.182#7513",
                    "372645a93b9cd222",
                ),
                selector: Some("192.168.31.182#7513:372645a93b9cd222".to_string()),
                availability: SessionAvailability::Online,
                workspace_dir: Some(PathBuf::from("/home/kk/wait-agent")),
                workspace_key: Some("372645a93b9cd222".to_string()),
                session_role: None,
                opened_by: Vec::new(),
                attached_clients: 0,
                window_count: 1,
                command_name: Some("bash".to_string()),
                current_path: Some(PathBuf::from("/home/kk/wait-agent")),
                task_state: ManagedSessionTaskState::Input,
            }],
            24,
            6,
            0,
        );

        assert!(output.contains("192.168.31.182 INPUT"));
        assert!(!output.contains("bash@192.168.31.182:3726"));
    }
}
