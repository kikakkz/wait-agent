use crate::domain::session_catalog::{ManagedSessionRecord, SessionTransport};
use crate::runtime::ratatui_remote_connect::connect_remote_host;
use std::sync::{Arc, Mutex};

use super::runtime::SharedState;
use super::snapshot::{ControlResponse, ServerStatus, SessionView};

pub(crate) fn is_one_shot_control_command(command: &str) -> bool {
    matches!(command, "STATUS" | "STOP" | "LIST_SESSIONS" | "DETACH_ALL")
        || command.starts_with("CONNECT_REMOTE_HOST ")
        || command.starts_with("RESIZE ")
        || command.starts_with("INPUT ")
}

pub(crate) fn response_should_broadcast(response: &ControlResponse) -> bool {
    response.broadcast
}

pub(crate) fn handle_control_command(
    command: &str,
    shared: &Arc<SharedState>,
    _stream: &mut std::os::unix::net::UnixStream,
) -> Option<ControlResponse> {
    if command == "STATUS" {
        let count = shared
            .client_count
            .load(std::sync::atomic::Ordering::SeqCst);
        let uptime = shared.start_time.elapsed().as_secs();
        let session_count = shared.sessions.lock().unwrap().len();
        let status = ServerStatus {
            port: shared.network.port,
            client_count: count,
            uptime_secs: uptime,
            session_count,
        };
        return Some(ControlResponse::ok_data(
            serde_json::to_value(&status).unwrap_or_default(),
        ));
    }

    if command == "STOP" {
        shared
            .shutdown
            .store(true, std::sync::atomic::Ordering::SeqCst);
        // Wake up the listener by connecting to it from this side.
        let _ = std::os::unix::net::UnixStream::connect(super::socket::ratatui_socket_path(
            shared.network.port,
        ));
        return Some(ControlResponse::ok_message("stopping"));
    }

    if command == "LIST_SESSIONS" {
        let guard = shared.sessions.lock().unwrap();
        let sessions: Vec<SessionView> = guard.values().map(SessionView::from_record).collect();
        drop(guard);
        return Some(ControlResponse::ok_data(
            serde_json::to_value(&sessions).unwrap_or_default(),
        ));
    }

    if command == "CREATE_LOCAL_SESSION" {
        let id = {
            let guard = shared.sessions.lock().unwrap();
            format!("{}", guard.len() + 1)
        };
        return match shared.create_local_session(&id, 80, 24) {
            Ok(target) => Some(
                ControlResponse::ok_message(format!("created local session {target}"))
                    .with_broadcast(),
            ),
            Err(error) => Some(ControlResponse::err(error.to_string())),
        };
    }

    if let Some(args) = command.strip_prefix("RESIZE ") {
        let mut parts = args.split_whitespace();
        let cols: u16 = parts.next().and_then(|v| v.parse().ok()).unwrap_or(80);
        let rows: u16 = parts.next().and_then(|v| v.parse().ok()).unwrap_or(24);
        let active = shared.active_target.lock().unwrap().clone();
        let transport = active.as_deref().and_then(|target| {
            let guard = shared.sessions.lock().unwrap();
            guard
                .values()
                .find(|s| s.address.qualified_target() == target)
                .map(|s| s.address.transport().clone())
        });
        match transport {
            Some(SessionTransport::RemotePeer) => {
                shared.resize_active_remote_session(cols, rows);
            }
            _ => {
                shared.resize_active_local_session(cols, rows);
            }
        }
        return Some(ControlResponse::ok());
    }

    if let Some(args) = command.strip_prefix("INPUT ") {
        let mut parts = args.splitn(2, ' ');
        let session_id = parts.next().unwrap_or("").to_string();
        let encoded = parts.next().unwrap_or("");
        match base64::decode(encoded) {
            Ok(bytes) => {
                let record = {
                    let guard = shared.sessions.lock().unwrap();
                    guard.get(&session_id).cloned()
                };
                match record.map(|r| r.address.transport().clone()) {
                    Some(SessionTransport::RemotePeer) => {
                        shared.feed_active_remote_session_input(bytes);
                    }
                    _ => {
                        shared.feed_local_session_input(&session_id, bytes);
                    }
                }
                return Some(ControlResponse::ok());
            }
            Err(_) => return Some(ControlResponse::err("invalid base64")),
        }
    }

    if let Some(target) = command.strip_prefix("ACTIVATE_TARGET ") {
        let target = target.to_string();
        let record = {
            let guard = shared.sessions.lock().unwrap();
            guard
                .values()
                .find(|session| session.address.qualified_target() == target)
                .cloned()
        };
        if let Some(record) = record {
            if record.address.transport() == &SessionTransport::RemotePeer {
                if let Err(error) = shared.ensure_remote_session(&record) {
                    return Some(ControlResponse::err(error.to_string()));
                }
            }
            *shared.active_target.lock().unwrap() = Some(target);
            return Some(ControlResponse::ok().with_broadcast());
        }
        return Some(ControlResponse::err("unknown target"));
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
                guard.insert(record.address.session_id().to_string(), record.clone());
                drop(guard);
                if let Err(error) = shared.ensure_remote_session(&record) {
                    return Some(ControlResponse::err(error.to_string()));
                }
                *shared.active_target.lock().unwrap() = Some(target.clone());
                Some(ControlResponse::ok_message(format!("connected {target}")).with_broadcast())
            }
            Err(error) => Some(ControlResponse::err(error.to_string())),
        }
    } else {
        None
    }
}
