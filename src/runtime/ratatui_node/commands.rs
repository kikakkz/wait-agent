use crate::domain::session_catalog::{
    ManagedSessionAddress, ManagedSessionRecord, ManagedSessionTaskState, SessionAvailability,
};
use crate::runtime::ratatui_remote_connect::connect_remote_host;
use std::sync::{Arc, Mutex};

use super::runtime::SharedState;
use super::snapshot::{ServerStatus, SessionView};

pub(crate) fn is_one_shot_control_command(command: &str) -> bool {
    matches!(command, "STATUS" | "STOP" | "LIST_SESSIONS" | "DETACH_ALL")
        || command.starts_with("CONNECT_REMOTE_HOST ")
}

pub(crate) fn response_should_broadcast(response: &str) -> bool {
    response.starts_with("OK")
}

pub(crate) fn handle_control_command(
    command: &str,
    shared: &Arc<SharedState>,
    _stream: &mut std::os::unix::net::UnixStream,
) -> Option<String> {
    if command == "STATUS" {
        let count = shared.client_count.load(std::sync::atomic::Ordering::SeqCst);
        let uptime = shared.start_time.elapsed().as_secs();
        let session_count = shared.sessions.lock().unwrap().len();
        let status = ServerStatus {
            port: shared.network.port,
            client_count: count,
            uptime_secs: uptime,
            session_count,
        };
        return Some(serde_json::to_string(&status).unwrap_or_default());
    }

    if command == "STOP" {
        shared
            .shutdown
            .store(true, std::sync::atomic::Ordering::SeqCst);
        // Wake up the listener by connecting to it from this side.
        let _ = std::os::unix::net::UnixStream::connect(super::socket::ratatui_socket_path(
            shared.network.port,
        ));
        return Some("OK stopping".to_string());
    }

    if command == "LIST_SESSIONS" {
        let guard = shared.sessions.lock().unwrap();
        let sessions: Vec<SessionView> = guard.values().map(SessionView::from_record).collect();
        drop(guard);
        return Some(serde_json::to_string(&sessions).unwrap_or_default());
    }

    if command == "CREATE_LOCAL_SESSION" {
        let mut guard = shared.sessions.lock().unwrap();
        let id = format!("{}", guard.len() + 1);
        let record = ManagedSessionRecord {
            address: ManagedSessionAddress::local_tmux(&id, "main"),
            selector: None,
            availability: SessionAvailability::Online,
            workspace_dir: None,
            workspace_key: None,
            session_role: None,
            opened_by: Vec::new(),
            attached_clients: 0,
            window_count: 1,
            command_name: Some("bash".to_string()),
            display_command_name: None,
            current_path: None,
            task_state: ManagedSessionTaskState::Running,
        };
        let target = record.address.qualified_target();
        guard.insert(id, record);
        drop(guard);
        *shared.active_target.lock().unwrap() = Some(target);
        return Some("OK created local session".to_string());
    }

    if let Some(target) = command.strip_prefix("ACTIVATE_TARGET ") {
        let target = target.to_string();
        let guard = shared.sessions.lock().unwrap();
        let exists = guard
            .values()
            .any(|session| session.address.qualified_target() == target);
        drop(guard);
        if exists {
            *shared.active_target.lock().unwrap() = Some(target);
            return Some("OK".to_string());
        }
        return Some("ERR unknown target".to_string());
    }

    if let Some(profile_name) = command.strip_prefix("CONNECT_REMOTE_HOST ") {
        // Build a temporary Vec view for the existing connect helper.
        let sessions_vec: Vec<ManagedSessionRecord> = {
            let guard = shared.sessions.lock().unwrap();
            guard.values().cloned().collect()
        };
        let sessions_arc = Arc::new(Mutex::new(sessions_vec));
        match connect_remote_host(profile_name, &sessions_arc, &shared.network) {
            Ok(record) => {
                let target = record.address.qualified_target();
                let mut guard = shared.sessions.lock().unwrap();
                guard.retain(|_, session| session.address.id() != record.address.id());
                guard.insert(record.address.session_id().to_string(), record);
                drop(guard);
                *shared.active_target.lock().unwrap() = Some(target.clone());
                Some(format!("OK connected {target}"))
            }
            Err(error) => Some(format!("ERR {error}")),
        }
    } else {
        None
    }
}
