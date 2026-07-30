use crate::domain::session_catalog::SessionTransport;
use crate::runtime::ratatui_node::state_event::{CommandOutcome, StateEvent};
use std::sync::mpsc;
use std::sync::Arc;

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
        let session_count = shared
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len();
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
        return Some(route_mutating_command(shared, |reply_tx| {
            StateEvent::ClientStop { reply_tx }
        }));
    }

    if command == "LIST_SESSIONS" {
        let guard = shared.sessions.lock().unwrap_or_else(|e| e.into_inner());
        let sessions: Vec<SessionView> = guard.values().map(SessionView::from_record).collect();
        drop(guard);
        return Some(ControlResponse::ok_data(
            serde_json::to_value(&sessions).unwrap_or_default(),
        ));
    }

    if command == "CREATE_LOCAL_SESSION" {
        return Some(route_mutating_command(shared, |reply_tx| {
            StateEvent::ClientCreateLocalSession { reply_tx }
        }));
    }

    if command == "DETACH_ALL" {
        return Some(route_mutating_command(shared, |reply_tx| {
            StateEvent::ClientDetachAll { reply_tx }
        }));
    }

    if let Some(args) = command.strip_prefix("RESIZE ") {
        let mut parts = args.split_whitespace();
        let cols: u16 = parts.next().and_then(|v| v.parse().ok()).unwrap_or(80);
        let rows: u16 = parts.next().and_then(|v| v.parse().ok()).unwrap_or(24);
        // Lock order: sessions -> active_target.
        let transport = {
            let guard = shared.sessions.lock().unwrap_or_else(|e| e.into_inner());
            let active = shared
                .active_target
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            active.and_then(|target| {
                guard
                    .values()
                    .find(|s| s.address.qualified_target() == target)
                    .map(|s| s.address.transport().clone())
            })
        };
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
                    let guard = shared.sessions.lock().unwrap_or_else(|e| e.into_inner());
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
        return Some(route_mutating_command(shared, |reply_tx| {
            StateEvent::ClientActivatedTarget {
                target_id: target,
                reply_tx,
            }
        }));
    }

    if let Some(profile_name) = command.strip_prefix("CONNECT_REMOTE_HOST ") {
        let profile_name = profile_name.to_string();
        return Some(route_mutating_command(shared, |reply_tx| {
            StateEvent::ClientConnectRemoteHost {
                profile_name,
                reply_tx,
            }
        }));
    }

    None
}

fn route_mutating_command<F>(shared: &Arc<SharedState>, build_event: F) -> ControlResponse
where
    F: FnOnce(mpsc::Sender<CommandOutcome>) -> StateEvent,
{
    let (reply_tx, reply_rx) = mpsc::channel::<CommandOutcome>();
    let event = build_event(reply_tx);
    if shared.state_sender().send(event).is_err() {
        return ControlResponse::err("state event loop is not running");
    }
    match reply_rx.recv() {
        Ok(CommandOutcome::Ok) => ControlResponse::ok(),
        Ok(CommandOutcome::Message(message)) => ControlResponse::ok_message(message),
        Ok(CommandOutcome::Error(message)) => ControlResponse::err(message),
        Err(_) => ControlResponse::err("state event loop dropped the reply"),
    }
}
