use crate::domain::session_catalog::ManagedSessionRecord;
use crate::runtime::ratatui_remote_connect::connect_remote_host;
use std::sync::{Arc, Mutex};

use super::runtime::SharedState;
use super::snapshot::{ServerStatus, SessionView};

pub(crate) fn is_one_shot_control_command(command: &str) -> bool {
    matches!(command, "STATUS" | "STOP" | "LIST_SESSIONS" | "DETACH_ALL")
        || command.starts_with("CONNECT_REMOTE_HOST ")
        || command.starts_with("RESIZE ")
        || command.starts_with("INPUT ")
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
        let id = {
            let guard = shared.sessions.lock().unwrap();
            format!("{}", guard.len() + 1)
        };
        match shared.create_local_session(&id, 80, 24) {
            Ok(target) => {
                return Some(format!("OK created local session {target}"));
            }
            Err(error) => {
                return Some(format!("ERR {error}"));
            }
        }
    }

    if let Some(args) = command.strip_prefix("RESIZE ") {
        let mut parts = args.split_whitespace();
        let cols: u16 = parts.next().and_then(|v| v.parse().ok()).unwrap_or(80);
        let rows: u16 = parts.next().and_then(|v| v.parse().ok()).unwrap_or(24);
        shared.resize_active_local_session(cols, rows);
        return Some("OK".to_string());
    }

    if let Some(args) = command.strip_prefix("INPUT ") {
        let mut parts = args.splitn(2, ' ');
        let session_id = parts.next().unwrap_or("").to_string();
        let encoded = parts.next().unwrap_or("");
        match base64::decode(encoded) {
            Ok(bytes) => {
                shared.feed_local_session_input(&session_id, bytes);
                return Some("OK".to_string());
            }
            Err(_) => return Some("ERR invalid base64".to_string()),
        }
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
