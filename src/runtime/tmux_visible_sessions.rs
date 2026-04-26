use crate::domain::session_catalog::ManagedSessionRecord;

pub(crate) fn visible_target_sessions(
    sessions: &[ManagedSessionRecord],
    workspace_session_name: &str,
    active_target: Option<&str>,
) -> Vec<ManagedSessionRecord> {
    let workspace_runtime = sessions
        .iter()
        .find(|session| session.address.session_id() == workspace_session_name)
        .cloned();
    let mut target_hosts = sessions
        .iter()
        .filter(|session| session.is_target_host())
        .cloned()
        .collect::<Vec<_>>();

    if target_hosts.is_empty() {
        return workspace_runtime.into_iter().collect();
    }

    if let Some(active_target) = active_target {
        if let Some(workspace_runtime) = workspace_runtime.as_ref() {
            if let Some(active_session) = target_hosts
                .iter_mut()
                .find(|session| session.address.qualified_target() == active_target)
            {
                active_session.command_name = workspace_runtime.command_name.clone();
                active_session.current_path = workspace_runtime.current_path.clone();
                active_session.task_state = workspace_runtime.task_state;
            }
        }
    }

    target_hosts
}

#[cfg(test)]
mod tests {
    use super::visible_target_sessions;
    use crate::domain::session_catalog::{
        ManagedSessionAddress, ManagedSessionRecord, ManagedSessionTaskState,
    };
    use crate::domain::workspace::WorkspaceSessionRole;
    use std::path::PathBuf;

    #[test]
    fn visible_target_sessions_keeps_target_hosts_visible_until_target_is_activated() {
        let sessions = visible_target_sessions(
            &[
                session(
                    "wa-1",
                    "workspace",
                    "codex",
                    WorkspaceSessionRole::WorkspaceChrome,
                ),
                session("wa-1", "target-1", "bash", WorkspaceSessionRole::TargetHost),
            ],
            "workspace",
            None,
        );

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].address.session_id(), "target-1");
        assert_eq!(sessions[0].command_name.as_deref(), Some("bash"));
    }

    #[test]
    fn visible_target_sessions_overlays_workspace_runtime_onto_active_target() {
        let sessions = visible_target_sessions(
            &[
                session(
                    "wa-1",
                    "workspace",
                    "codex",
                    WorkspaceSessionRole::WorkspaceChrome,
                ),
                session("wa-1", "target-1", "bash", WorkspaceSessionRole::TargetHost),
                session("wa-1", "target-2", "bash", WorkspaceSessionRole::TargetHost),
            ],
            "workspace",
            Some("wa-1:target-2"),
        );

        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[1].address.session_id(), "target-2");
        assert_eq!(sessions[1].command_name.as_deref(), Some("codex"));
    }

    fn session(
        socket: &str,
        session: &str,
        command: &str,
        role: WorkspaceSessionRole,
    ) -> ManagedSessionRecord {
        ManagedSessionRecord {
            address: ManagedSessionAddress::local_tmux(socket, session),
            workspace_dir: Some(PathBuf::from("/tmp/demo")),
            workspace_key: None,
            session_role: Some(role),
            attached_clients: 1,
            window_count: 1,
            command_name: Some(command.to_string()),
            current_path: Some(PathBuf::from("/tmp/demo")),
            task_state: ManagedSessionTaskState::Input,
        }
    }
}
