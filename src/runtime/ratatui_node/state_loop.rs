use crate::infra::error_log::ERROR_LOG;
use crate::lifecycle::LifecycleError;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use super::authority_host_io_loop::{AuthorityHostIoLoop, AuthorityHostIoRequest};
use super::runtime::SharedState;
use super::state_event::{CommandOutcome, CreatedAuthorityHostTarget, StateEvent};
use crate::domain::session_catalog::{ManagedSessionRecord, SessionTransport};
use crate::runtime::ratatui_remote_connect::connect_remote_host;
use crate::runtime::remote_node_session_sync_runtime::{
    LocalCatalogChangeReason, LocalCatalogChangeRequest,
};

/// Single thread that owns all writes to `SharedState`.
///
/// Local session lifecycle events, authority-host child exits, session sync
/// creation requests, and remote viewer close notifications all converge here.
/// Raw PTY data is forwarded by `AuthorityHostIoLoop` and does not pass through
/// this channel.
pub(crate) struct StateEventLoop {
    tx: mpsc::Sender<StateEvent>,
}

impl StateEventLoop {
    pub(crate) fn start(
        shared: Arc<SharedState>,
        catalog_tx: mpsc::Sender<LocalCatalogChangeRequest>,
        authority_host_io: &AuthorityHostIoLoop,
    ) -> Result<Self, LifecycleError> {
        let (tx, rx) = mpsc::channel::<StateEvent>();
        let authority_host_io_tx = authority_host_io.sender();
        std::thread::spawn(move || {
            if let Err(error) = run_state_event_loop(shared, rx, catalog_tx, authority_host_io_tx) {
                ERROR_LOG.log(format!(
                    "[ratatui-state-loop] loop exited with error: {error}"
                ));
            }
        });
        Ok(Self { tx })
    }

    pub(crate) fn sender(&self) -> mpsc::Sender<StateEvent> {
        self.tx.clone()
    }
}

fn run_state_event_loop(
    shared: Arc<SharedState>,
    rx: mpsc::Receiver<StateEvent>,
    catalog_tx: mpsc::Sender<LocalCatalogChangeRequest>,
    authority_host_io_tx: mpsc::Sender<AuthorityHostIoRequest>,
) -> Result<(), LifecycleError> {
    while let Ok(event) = rx.recv() {
        match event {
            StateEvent::LocalSessionChildExit { target_id, .. } => {
                ERROR_LOG.log(format!(
                    "[ratatui-state-loop] local session exit target_id={target_id}"
                ));
                // AuthorityHostIoLoop uses short session ids; local sessions are
                // managed by alacritty_terminal and ignore the unregister request.
                if let Some((_, session_id)) = target_id.rsplit_once(':') {
                    let _ = authority_host_io_tx.send(AuthorityHostIoRequest::UnregisterSession {
                        session_id: session_id.to_string(),
                    });
                }
                shared.handle_session_exit(&target_id);
                let _ = catalog_tx.send(LocalCatalogChangeRequest {
                    reason: LocalCatalogChangeReason::LocalTargetExited {
                        target_session_name: target_id,
                    },
                    ack_tx: None,
                });
                let _ = shared.broadcast_snapshot();
            }

            StateEvent::AuthorityHostSessionChildExited { target_id, .. }
            | StateEvent::AuthorityHostSessionPtyClosed { target_id } => {
                // AuthorityHostIoLoop reports the short session id; turn it into
                // the local qualified target before touching SharedState.
                let qualified_target = format!("{}:{target_id}", shared.local_authority_id());
                ERROR_LOG.log(format!(
                    "[ratatui-state-loop] authority host session exit target_id={qualified_target}"
                ));
                let _ = authority_host_io_tx.send(AuthorityHostIoRequest::UnregisterSession {
                    session_id: target_id,
                });
                shared.handle_session_exit(&qualified_target);
                let _ = catalog_tx.send(LocalCatalogChangeRequest {
                    reason: LocalCatalogChangeReason::LocalTargetExited {
                        target_session_name: qualified_target,
                    },
                    ack_tx: None,
                });
                let _ = shared.broadcast_snapshot();
            }

            StateEvent::LocalSessionTitleChanged { target_id, title } => {
                shared.set_local_session_title(&target_id, title);
                let _ = shared.broadcast_snapshot();
            }

            StateEvent::LocalSessionOutput { .. } => {
                let _ = shared.broadcast_snapshot();
            }

            StateEvent::ClientConnected { client_id } => {
                shared
                    .client_count
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let _ = shared.broadcast_snapshot();
                let _ = client_id;
            }

            StateEvent::ClientDisconnected { client_id } => {
                shared
                    .client_count
                    .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                let _ = client_id;
            }

            StateEvent::ClientInput { target_id, bytes } => {
                route_input(&shared, &authority_host_io_tx, &target_id, bytes);
            }

            StateEvent::ClientActivatedTarget {
                target_id,
                reply_tx,
            } => {
                let outcome = activate_target(&shared, &target_id);
                let _ = reply_tx.send(outcome.clone());
                if matches!(outcome, CommandOutcome::Ok) {
                    let _ = shared.broadcast_snapshot();
                }
            }

            StateEvent::ClientResized { cols, rows } => {
                shared.resize_active_local_session(cols, rows);
                if let Some(id) = active_authority_host_session_id(&shared) {
                    let _ = authority_host_io_tx.send(AuthorityHostIoRequest::Resize {
                        session_id: id,
                        cols,
                        rows,
                    });
                }
            }

            StateEvent::ClientCreateLocalSession { reply_tx } => {
                let id = {
                    let guard = shared.sessions.lock().unwrap_or_else(|e| e.into_inner());
                    format!("{}", guard.len() + 1)
                };
                let outcome = match shared.create_local_session(&id, 80, 24) {
                    Ok(target) => {
                        CommandOutcome::Message(format!("created local session {target}"))
                    }
                    Err(error) => CommandOutcome::Error(error.to_string()),
                };
                let _ = reply_tx.send(outcome);
                let _ = shared.broadcast_snapshot();
            }

            StateEvent::ClientStop { reply_tx } => {
                shared
                    .shutdown
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                let _ = std::os::unix::net::UnixStream::connect(
                    super::socket::ratatui_socket_path(shared.network.port),
                );
                let _ = reply_tx.send(CommandOutcome::Message("stopping".to_string()));
            }

            StateEvent::ClientConnectRemoteHost {
                profile_name,
                reply_tx,
            } => {
                let outcome = connect_remote_host_target(&shared, &profile_name);
                let _ = reply_tx.send(outcome.clone());
                if matches!(outcome, CommandOutcome::Message(_)) {
                    let _ = shared.broadcast_snapshot();
                }
            }

            StateEvent::ClientDetachAll { reply_tx } => {
                shared.detach_all_clients();
                let _ = reply_tx.send(CommandOutcome::Ok);
                let _ = shared.broadcast_snapshot();
            }

            StateEvent::CreateAuthorityHostSession {
                request_id,
                cols,
                rows,
                reply_tx,
            } => {
                let reply =
                    create_authority_host_session(&shared, &authority_host_io_tx, cols, rows);
                let reply_for_log = reply
                    .as_ref()
                    .map(|created| format!("{}->{}", created.session_id, created.target_id))
                    .unwrap_or_else(|error| format!("error:{error}"));
                ERROR_LOG.log(format!(
                    "[ratatui-state-loop] create authority host session request_id={request_id} result={reply_for_log}"
                ));
                let is_ok = reply.is_ok();
                let _ = reply_tx.send(reply);
                if is_ok {
                    let _ = catalog_tx.send(LocalCatalogChangeRequest {
                        reason: LocalCatalogChangeReason::LocalRuntimeChanged,
                        ack_tx: None,
                    });
                    let _ = shared.broadcast_snapshot();
                }
            }

            StateEvent::RemoteSessionOutput { .. } | StateEvent::RemoteSessionInputEcho { .. } => {
                let _ = shared.broadcast_snapshot();
            }

            StateEvent::RemoteSessionClosed { target_id } => {
                shared.handle_session_exit(&target_id);
                let _ = shared.broadcast_snapshot();
            }
        }
    }
    Ok(())
}

fn create_authority_host_session(
    shared: &Arc<SharedState>,
    authority_host_io_tx: &mpsc::Sender<AuthorityHostIoRequest>,
    cols: u16,
    rows: u16,
) -> Result<CreatedAuthorityHostTarget, LifecycleError> {
    let (session_id, mut session, target_id) = shared.create_authority_host_session(cols, rows)?;
    let pty_master = session.pty_master.try_clone().map_err(|error| {
        LifecycleError::Io(
            "failed to clone authority host pty master".to_string(),
            error,
        )
    })?;
    let child = session.child.take().ok_or_else(|| {
        LifecycleError::Protocol("authority host session missing child process".to_string())
    })?;
    let _ = authority_host_io_tx.send(AuthorityHostIoRequest::RegisterSession {
        session_id: session_id.clone(),
        pty_master,
        child,
        output_tx: None,
    });
    {
        let mut host_guard = shared
            .authority_host_sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        host_guard.insert(session_id.clone(), Arc::new(session));
    }
    Ok(CreatedAuthorityHostTarget {
        session_id,
        target_id,
    })
}

fn route_input(
    shared: &SharedState,
    authority_host_io_tx: &mpsc::Sender<AuthorityHostIoRequest>,
    target_id: &str,
    bytes: Vec<u8>,
) {
    let record = {
        let guard = shared.sessions.lock().unwrap_or_else(|e| e.into_inner());
        guard.get(target_id).cloned()
    };
    let Some(record) = record else {
        return;
    };
    match record.address.transport() {
        SessionTransport::Local => {
            // Local PTY sessions are keyed by qualified target.
            if shared
                .local_sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains_key(target_id)
            {
                shared.feed_local_session_input(target_id, bytes);
                return;
            }
            // Authority-host sessions are keyed by short session id inside the
            // IO loop.
            if let Some(session_id) = target_id.rsplit_once(':').map(|(_, id)| id.to_string()) {
                if shared
                    .authority_host_sessions
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .contains_key(&session_id)
                {
                    let _ = authority_host_io_tx
                        .send(AuthorityHostIoRequest::WriteInput { session_id, bytes });
                }
            }
        }
        SessionTransport::RemotePeer => {
            shared.feed_remote_session_input(target_id, bytes);
        }
    }
}

fn active_authority_host_session_id(shared: &SharedState) -> Option<String> {
    // Lock order: sessions -> active_target -> authority_host_sessions.
    let guard = shared.sessions.lock().unwrap_or_else(|e| e.into_inner());
    let active = shared
        .active_target
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()?;
    let record = guard
        .values()
        .find(|r| r.address.qualified_target() == active)?;
    let session_id = record.address.session_id().to_string();
    let is_host = shared
        .authority_host_sessions
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains_key(&session_id);
    if is_host {
        Some(session_id)
    } else {
        None
    }
}

fn activate_target(shared: &Arc<SharedState>, target_id: &str) -> CommandOutcome {
    let record = {
        let guard = shared.sessions.lock().unwrap_or_else(|e| e.into_inner());
        guard
            .values()
            .find(|s| s.address.qualified_target() == target_id)
            .cloned()
    };
    if let Some(record) = record {
        if *record.address.transport() == SessionTransport::RemotePeer {
            if let Err(error) = shared.ensure_remote_session(&record) {
                return CommandOutcome::Error(error.to_string());
            }
        }
        *shared
            .active_target
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(target_id.to_string());
        CommandOutcome::Ok
    } else {
        CommandOutcome::Error("unknown target".to_string())
    }
}

fn connect_remote_host_target(shared: &Arc<SharedState>, profile_name: &str) -> CommandOutcome {
    let sessions_vec: Vec<ManagedSessionRecord> = {
        let guard = shared.sessions.lock().unwrap_or_else(|e| e.into_inner());
        guard.values().cloned().collect()
    };
    let sessions_arc = Arc::new(Mutex::new(sessions_vec));
    match connect_remote_host(profile_name, &sessions_arc, &shared.network) {
        Ok(record) => {
            let target = record.address.qualified_target();
            {
                let mut guard = shared.sessions.lock().unwrap_or_else(|e| e.into_inner());
                guard.retain(|_, session| session.address.id() != record.address.id());
                guard.insert(target.clone(), record.clone());
            }
            if let Err(error) = shared.ensure_remote_session(&record) {
                return CommandOutcome::Error(error.to_string());
            }
            *shared
                .active_target
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(target.clone());
            CommandOutcome::Message(format!("connected {target}"))
        }
        Err(error) => CommandOutcome::Error(error.to_string()),
    }
}
