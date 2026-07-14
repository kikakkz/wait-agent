use crate::domain::chrome::FooterViewModel;
use crate::domain::session_catalog::ManagedSessionRecord;
use crate::ui::chrome::{display_width, style_status_line, truncate_display_width};

pub struct FooterUi;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FooterProjection {
    Pane,
    FullscreenStatus,
}

impl FooterUi {
    #[allow(dead_code)]
    pub fn render_view_model(model: &FooterViewModel) -> String {
        if model.fullscreen {
            Self::render_fullscreen(
                &model.active_socket,
                &model.active_session,
                model.active_target.as_deref(),
                &model.sessions,
                model.width,
                model.listener_display.as_deref(),
                model.connect_endpoint.as_deref(),
            )
        } else {
            Self::render(
                &model.active_socket,
                &model.active_session,
                model.active_target.as_deref(),
                &model.sessions,
                model.width,
                model.listener_display.as_deref(),
                model.connect_endpoint.as_deref(),
            )
        }
    }

    pub fn render(
        active_socket: &str,
        active_session: &str,
        active_target: Option<&str>,
        sessions: &[ManagedSessionRecord],
        width: usize,
        listener_display: Option<&str>,
        connect_endpoint: Option<&str>,
    ) -> String {
        render_footer(
            active_socket,
            active_session,
            active_target,
            sessions,
            width,
            FooterProjection::Pane,
            listener_display,
            connect_endpoint,
        )
    }

    pub fn render_fullscreen(
        active_socket: &str,
        active_session: &str,
        active_target: Option<&str>,
        sessions: &[ManagedSessionRecord],
        width: usize,
        listener_display: Option<&str>,
        connect_endpoint: Option<&str>,
    ) -> String {
        render_footer(
            active_socket,
            active_session,
            active_target,
            sessions,
            width,
            FooterProjection::FullscreenStatus,
            listener_display,
            connect_endpoint,
        )
    }
}

fn render_footer(
    active_socket: &str,
    active_session: &str,
    active_target: Option<&str>,
    sessions: &[ManagedSessionRecord],
    width: usize,
    projection: FooterProjection,
    listener_display: Option<&str>,
    connect_endpoint: Option<&str>,
) -> String {
    let width = width.max(1);
    let active = active_session_record(active_socket, active_session, active_target, sessions);
    let left = left_status_text(projection, listener_display, connect_endpoint);
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
    active_target: Option<&str>,
    sessions: &'a [ManagedSessionRecord],
) -> Option<&'a ManagedSessionRecord> {
    active_target
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

fn left_status_text(
    projection: FooterProjection,
    listener_display: Option<&str>,
    connect_endpoint: Option<&str>,
) -> String {
    let base = match projection {
        FooterProjection::Pane => {
            "Ctrl-N New · Ctrl-W Conn · Ctrl-S Remote · Ctrl-O Hist · Ctrl-E Logs · Ctrl-M Menu"
                .to_string()
        }
        FooterProjection::FullscreenStatus => {
            "History  Ctrl-O/Esc Exit · PgUp/PgDn Page · Up/Down Line · q Close · Ctrl-N New"
                .to_string()
        }
    };
    let mut parts: Vec<&str> = Vec::new();
    if let Some(listener) = listener_display {
        parts.push("Listen");
        parts.push(listener);
    }
    if let Some(connect) = connect_endpoint {
        parts.push("Connect");
        parts.push(connect);
    }
    if parts.is_empty() {
        return base;
    }
    format!("{base}  {}", parts.join("  "))
}

fn join_left_right(left: &str, right: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }

    let right_width = display_width(right).min(width.saturating_div(2).max(16));
    let right = truncate_path_from_left(right, right_width.min(width));
    let right_len = display_width(&right);
    if right_len >= width {
        return right;
    }

    let spacer = 1;
    let left_width = width.saturating_sub(right_len + spacer);
    let left = truncate_display_width(left, left_width);
    let padding = left_width.saturating_sub(display_width(&left));
    format!("{}{} {}", left, " ".repeat(padding), right)
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
            Some("wa-1:waitagent-1"),
            &[ManagedSessionRecord {
                address: ManagedSessionAddress::local_tmux("wa-1", "waitagent-1"),
                selector: Some("wa-1:waitagent-1".to_string()),
                availability: crate::domain::session_catalog::SessionAvailability::Online,
                workspace_dir: Some(PathBuf::from("/tmp/demo")),
                workspace_key: None,
                session_role: None,
                opened_by: Vec::new(),
                attached_clients: 1,
                window_count: 1,
                command_name: Some("codex".to_string()),
                display_command_name: None,
                current_path: Some(PathBuf::from("/tmp/demo")),
                task_state: ManagedSessionTaskState::Input,
            }],
            96,
            None,
            None,
        );

        assert!(output.contains("Ctrl-N"));
        assert!(output.contains("New"));
        assert!(output.contains("Ctrl-W"));
        assert!(output.contains("Conn"));
        assert!(output.contains("Ctrl-S"));
        assert!(output.contains("Remote"));
        assert!(output.contains("Ctrl-O"));
        assert!(output.contains("Hist"));
        assert!(output.contains("Ctrl-M"));
        assert!(output.contains("Menu"));
        assert!(!output.contains("Prefix-s"));
        assert!(!output.contains("Actions"));
        assert!(!output.contains("listen:"));
        assert!(!output.contains("total:"));
        assert!(!output.contains("R:"));
        assert!(output.contains("/tmp/demo"));
        assert!(!output.contains('\n'));
        assert!(output.starts_with("\u{1b}[48;5;24m"));
    }

    #[test]
    fn footer_ui_renders_modern_network_status() {
        let output = FooterUi::render(
            "wa-1",
            "waitagent-1",
            None,
            &[],
            120,
            Some("10.1.26.84:7474"),
            None,
        );

        assert!(output.contains("Ctrl-N"));
        assert!(output.contains("Ctrl-W"));
        assert!(output.contains("Ctrl-S"));
        assert!(output.contains("Listen"));
        assert!(output.contains("10.1.26.84:7474"));
        assert!(!output.contains("keys:"));
        assert!(!output.contains("Listen:"));
        assert!(!output.contains("Network"));
        assert!(!output.contains("│"));
        assert!(!output.contains('\n'));
    }

    #[test]
    fn history_footer_ui_shows_page_and_exit_keys() {
        let output = FooterUi::render_fullscreen(
            "wa-1",
            "waitagent-1",
            Some("wa-1:waitagent-1"),
            &[
                ManagedSessionRecord {
                    address: ManagedSessionAddress::local_tmux("wa-1", "waitagent-1"),
                    selector: Some("wa-1:waitagent-1".to_string()),
                    availability: crate::domain::session_catalog::SessionAvailability::Online,
                    workspace_dir: Some(PathBuf::from("/tmp/demo")),
                    workspace_key: None,
                    session_role: None,
                    opened_by: Vec::new(),
                    attached_clients: 1,
                    window_count: 1,
                    command_name: Some("bash".to_string()),
                    display_command_name: None,
                    current_path: Some(PathBuf::from("/tmp/demo")),
                    task_state: ManagedSessionTaskState::Input,
                },
                ManagedSessionRecord {
                    address: ManagedSessionAddress::local_tmux("wa-2", "waitagent-2"),
                    selector: Some("wa-2:waitagent-2".to_string()),
                    availability: crate::domain::session_catalog::SessionAvailability::Online,
                    workspace_dir: Some(PathBuf::from("/tmp/other")),
                    workspace_key: None,
                    session_role: None,
                    opened_by: Vec::new(),
                    attached_clients: 1,
                    window_count: 1,
                    command_name: Some("codex".to_string()),
                    display_command_name: None,
                    current_path: Some(PathBuf::from("/tmp/other")),
                    task_state: ManagedSessionTaskState::Confirm,
                },
            ],
            180,
            None,
            None,
        );

        assert!(output.contains("Ctrl-O"));
        assert!(output.contains("Esc"));
        assert!(output.contains("Exit"));
        assert!(output.contains("PgUp/PgDn"));
        assert!(output.contains("Up/Down"));
        assert!(output.contains("q"));
        assert!(output.contains("Close"));
        assert!(output.contains("Ctrl-N"));
        assert!(!output.contains("listen:"));
        assert!(!output.contains("total:"));
        assert!(!output.contains("R:0"));
        assert!(output.contains("/tmp/demo"));
    }
}
