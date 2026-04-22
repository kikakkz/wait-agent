use crate::domain::session_catalog::ManagedSessionRecord;
use std::fmt::Write;

pub struct SidebarUi;

impl SidebarUi {
    pub fn render(
        active_socket: &str,
        active_session: &str,
        sessions: &[ManagedSessionRecord],
    ) -> String {
        let mut buffer = String::new();
        let _ = writeln!(buffer, "WaitAgent");
        let _ = writeln!(buffer, "Sessions");
        let _ = writeln!(buffer, "----------------");

        if sessions.is_empty() {
            let _ = writeln!(buffer, "(no sessions)");
            return buffer;
        }

        for session in sessions {
            let is_active = session.address.server_id() == active_socket
                && session.address.session_id() == active_session;
            let marker = if is_active { '>' } else { ' ' };
            let label = short_session_label(session);
            let _ = writeln!(buffer, "{marker} {label} [{}]", session.attached_clients);
        }

        buffer
    }
}

fn short_session_label(session: &ManagedSessionRecord) -> String {
    session.address.display_session_id().to_string()
}

#[cfg(test)]
mod tests {
    use super::SidebarUi;
    use crate::domain::session_catalog::{ManagedSessionAddress, ManagedSessionRecord};
    use std::path::PathBuf;

    #[test]
    fn sidebar_ui_marks_active_session_and_uses_session_display_id() {
        let output = SidebarUi::render(
            "wa-1",
            "waitagent-1",
            &[ManagedSessionRecord {
                address: ManagedSessionAddress::local_tmux("wa-1", "waitagent-1"),
                workspace_dir: Some(PathBuf::from("/tmp/demo")),
                workspace_key: Some("1234".to_string()),
                attached_clients: 2,
                window_count: 1,
            }],
        );

        assert!(output.contains("WaitAgent"));
        assert!(output.contains("> 1 [2]"));
    }
}
