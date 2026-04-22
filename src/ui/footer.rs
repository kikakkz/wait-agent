use crate::domain::session_catalog::ManagedSessionRecord;

pub struct FooterUi;

impl FooterUi {
    pub fn render(
        active_socket: &str,
        active_session: &str,
        sessions: &[ManagedSessionRecord],
    ) -> String {
        let active_label = active_session_label(active_session, sessions);
        let attached = sessions
            .iter()
            .find(|session| {
                session.address.server_id() == active_socket
                    && session.address.session_id() == active_session
            })
            .map(|session| session.attached_clients)
            .unwrap_or(0);
        format!(
            "WaitAgent Menu | [Enter] switch  [h] sidebar  [z] zoom  [[] scroll  [d] detach | sessions={} active={} attached={}",
            sessions.len(),
            active_label,
            attached
        )
    }
}

fn active_session_label(active_session: &str, sessions: &[ManagedSessionRecord]) -> String {
    sessions
        .iter()
        .find(|session| session.address.session_id() == active_session)
        .map(|session| session.address.display_session_id().to_string())
        .unwrap_or_else(|| active_session.to_string())
}

#[cfg(test)]
mod tests {
    use super::FooterUi;

    #[test]
    fn footer_ui_renders_key_hints() {
        let output = FooterUi::render("wa-1", "waitagent-1", &[]);

        assert!(output.contains("WaitAgent Menu | [Enter] switch"));
        assert!(output.contains("sessions=0 active=waitagent-1 attached=0"));
        assert!(!output.contains("socket=wa-1"));
        assert!(!output.contains('\n'));
    }
}
