use crate::cli::RemoteNetworkConfig;
use crate::domain::session_catalog::ManagedSessionRecord;
use crate::infra::error_log::ERROR_LOG;
use crate::lifecycle::LifecycleError;
use crate::ratatui_node::remote_session::RatatuiRemoteSession;
use crate::ratatui_node::runtime::SharedState;
use crate::ratatui_node::state_event::StateEvent;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

/// Background worker that re-establishes the authority transport for a remote
/// session after it disconnects.
///
/// It runs outside `StateEventLoop` so the single writer is never blocked by
/// network I/O.  Progress is reported back as `StateEvent`s:
///
/// * `RemoteSessionReconnected` when the handshake completes.
/// * `SessionClosed` when the worker is cancelled or gives up.
pub(crate) struct ReconnectWorker {
    pub(crate) cancel_tx: mpsc::Sender<()>,
}

impl ReconnectWorker {
    /// Start a worker that attempts to reconnect `record` until it succeeds,
    /// is cancelled, or hits the maximum retry budget.
    pub(crate) fn start(
        record: ManagedSessionRecord,
        workspace_id: String,
        network: RemoteNetworkConfig,
        cols: u16,
        rows: u16,
        shared: Arc<SharedState>,
        state_tx: mpsc::Sender<StateEvent>,
    ) -> Self {
        let (cancel_tx, cancel_rx) = mpsc::channel::<()>();
        thread::spawn(move || {
            run_reconnect_worker(
                record,
                workspace_id,
                network,
                cols,
                rows,
                shared,
                state_tx,
                cancel_rx,
            );
        });
        Self { cancel_tx }
    }
}

#[allow(clippy::too_many_arguments)]
fn run_reconnect_worker(
    record: ManagedSessionRecord,
    workspace_id: String,
    network: RemoteNetworkConfig,
    cols: u16,
    rows: u16,
    shared: Arc<SharedState>,
    state_tx: mpsc::Sender<StateEvent>,
    cancel_rx: mpsc::Receiver<()>,
) {
    let target_id = record.address.qualified_target();
    let mut attempt: u32 = 0;
    let mut backoff = Duration::from_millis(500);
    const MAX_ATTEMPTS: u32 = 30;
    const MAX_BACKOFF: Duration = Duration::from_secs(30);
    const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

    loop {
        if cancel_rx.try_recv().is_ok() {
            ERROR_LOG.log(format!(
                "[reconnect-worker] {target_id} cancelled before attempt {}",
                attempt + 1
            ));
            let _ = state_tx.send(StateEvent::SessionClosed {
                target_id: target_id.clone(),
            });
            return;
        }

        attempt += 1;
        ERROR_LOG.log(format!(
            "[reconnect-worker] {target_id} attempt {attempt} backoff={backoff:?}"
        ));

        let (connected_tx, connected_rx) = mpsc::channel::<Result<(), LifecycleError>>();
        match RatatuiRemoteSession::open(
            &record,
            &workspace_id,
            &network,
            &shared,
            Some(connected_tx),
        ) {
            Ok(session) => {
                let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
                let remaining = deadline.saturating_duration_since(Instant::now());
                let handshake = connected_rx.recv_timeout(remaining);
                match handshake {
                    Ok(Ok(())) => {
                        ERROR_LOG.log(format!(
                            "[reconnect-worker] {target_id} handshake succeeded on attempt {attempt}"
                        ));
                        session.clear_output_seq();
                        session.open_mirror(cols, rows);
                        // Tell StateEventLoop to adopt the new transport.
                        let _ = state_tx.send(StateEvent::RemoteSessionReconnected {
                            target_id: target_id.clone(),
                            session,
                        });
                        return;
                    }
                    Ok(Err(error)) => {
                        ERROR_LOG.log(format!(
                            "[reconnect-worker] {target_id} handshake failed on attempt {attempt}: {error}"
                        ));
                        session.stop();
                    }
                    Err(RecvTimeoutError::Timeout) => {
                        ERROR_LOG.log(format!(
                            "[reconnect-worker] {target_id} handshake timeout on attempt {attempt}"
                        ));
                        session.stop();
                    }
                    Err(RecvTimeoutError::Disconnected) => {
                        ERROR_LOG.log(format!(
                            "[reconnect-worker] {target_id} handshake channel dropped on attempt {attempt}"
                        ));
                        session.stop();
                    }
                }
            }
            Err(error) => {
                ERROR_LOG.log(format!(
                    "[reconnect-worker] {target_id} open failed on attempt {attempt}: {error}"
                ));
            }
        }

        if attempt >= MAX_ATTEMPTS {
            ERROR_LOG.log(format!(
                "[reconnect-worker] {target_id} giving up after {attempt} attempts"
            ));
            let _ = state_tx.send(StateEvent::SessionClosed {
                target_id: target_id.clone(),
            });
            return;
        }

        // Wait before the next attempt, but exit early if cancelled.
        match cancel_rx.recv_timeout(backoff) {
            Ok(()) | Err(RecvTimeoutError::Disconnected) => {
                ERROR_LOG.log(format!(
                    "[reconnect-worker] {target_id} cancelled during backoff after attempt {attempt}"
                ));
                let _ = state_tx.send(StateEvent::SessionClosed {
                    target_id: target_id.clone(),
                });
                return;
            }
            Err(RecvTimeoutError::Timeout) => {}
        }

        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}
