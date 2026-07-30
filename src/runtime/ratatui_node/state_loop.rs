use crate::infra::error_log::ERROR_LOG;
use crate::lifecycle::LifecycleError;
use std::sync::mpsc;
use std::sync::Arc;

use super::authority_host_io_loop::{AuthorityHostIoLoop, AuthorityHostIoRequest};
use super::runtime::SharedState;
use super::state_event::{CreatedAuthorityHostTarget, StateEvent};
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
                ERROR_LOG.log(format!("[ratatui-state-loop] loop exited with error: {error}"));
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
            StateEvent::LocalSessionChildExit { session_id, .. }
            | StateEvent::AuthorityHostSessionChildExited { session_id, .. }
            | StateEvent::AuthorityHostSessionPtyClosed { session_id } => {
                ERROR_LOG.log(format!(
                    "[ratatui-state-loop] session exit event session={session_id}"
                ));
                // Make sure AuthorityHostIoLoop stops polling this session's fd
                // and child.  Local sessions ignore the unregister request because
                // they are managed by alacritty_terminal's event loop.
                let _ = authority_host_io_tx.send(AuthorityHostIoRequest::UnregisterSession {
                    session_id: session_id.clone(),
                });
                shared.handle_session_exit(&session_id);
                let _ = catalog_tx.send(LocalCatalogChangeRequest {
                    reason: LocalCatalogChangeReason::LocalTargetExited {
                        target_session_name: session_id,
                    },
                    ack_tx: None,
                });
                let _ = shared.broadcast_snapshot();
            }

            StateEvent::LocalSessionTitleChanged { session_id, title } => {
                shared.set_local_session_title(&session_id, title);
                let _ = shared.broadcast_snapshot();
            }

            StateEvent::ClientConnected { client_id } => {
                shared.client_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let _ = shared.broadcast_snapshot();
                let _ = client_id;
            }

            StateEvent::ClientDisconnected { client_id } => {
                shared.client_count.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                let _ = client_id;
            }

            StateEvent::ClientInput { session_id, bytes } => {
                if is_local_session(&shared, &session_id) {
                    shared.feed_local_session_input(&session_id, bytes);
                } else {
                    let _ = authority_host_io_tx.send(AuthorityHostIoRequest::WriteInput {
                        session_id,
                        bytes,
                    });
                }
            }

            StateEvent::ClientActivatedTarget { target_id } => {
                *shared.active_target.lock().unwrap() = Some(target_id);
                let _ = shared.broadcast_snapshot();
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

            StateEvent::ClientCreateLocalSession => {
                let id = {
                    let guard = shared.sessions.lock().unwrap();
                    format!("{}", guard.len() + 1)
                };
                let _ = shared.create_local_session(&id, 80, 24);
                let _ = catalog_tx.send(LocalCatalogChangeRequest {
                    reason: LocalCatalogChangeReason::LocalRuntimeChanged,
                    ack_tx: None,
                });
                let _ = shared.broadcast_snapshot();
            }

            StateEvent::CreateAuthorityHostSession {
                request_id,
                cols,
                rows,
                reply_tx,
            } => {
                let reply = create_authority_host_session(
                    &shared,
                    &authority_host_io_tx,
                    cols,
                    rows,
                );
                let reply_for_log = reply
                    .as_ref()
                    .map(|created| format!("{}->{}" , created.session_id, created.target_id))
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

            StateEvent::RemoteSessionClosed { target_id } => {
                let session_id = {
                    let guard = shared.sessions.lock().unwrap();
                    guard
                        .iter()
                        .find(|(_, session)| session.address.qualified_target() == target_id)
                        .map(|(session_id, _)| session_id.clone())
                };
                if let Some(session_id) = session_id {
                    shared.handle_session_exit(&session_id);
                }
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
        LifecycleError::Protocol(
            "authority host session missing child process".to_string(),
        )
    })?;
    let _ = authority_host_io_tx.send(AuthorityHostIoRequest::RegisterSession {
        session_id: session_id.clone(),
        pty_master,
        child,
        output_tx: None,
    });
    {
        let mut host_guard = shared.authority_host_sessions.lock().unwrap();
        host_guard.insert(session_id.clone(), Arc::new(session));
    }
    Ok(CreatedAuthorityHostTarget {
        session_id,
        target_id,
    })
}

fn is_local_session(shared: &SharedState, session_id: &str) -> bool {
    let guard = shared.sessions.lock().unwrap();
    guard
        .get(session_id)
        .map(|record| *record.address.transport() == crate::domain::session_catalog::SessionTransport::LocalTmux)
        .unwrap_or(false)
}

fn active_authority_host_session_id(shared: &SharedState) -> Option<String> {
    let active = shared.active_target.lock().unwrap().clone()?;
    let guard = shared.sessions.lock().unwrap();
    let record = guard.values().find(|r| r.address.qualified_target() == active)?;
    let session_id = record.address.session_id().to_string();
    let is_host = shared
        .authority_host_sessions
        .lock()
        .unwrap()
        .contains_key(&session_id);
    if is_host {
        Some(session_id)
    } else {
        None
    }
}
