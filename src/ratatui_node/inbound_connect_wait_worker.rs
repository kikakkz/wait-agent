use crate::infra::error_log::ERROR_LOG;
use crate::ratatui_node::state_event::StateEvent;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

/// Background worker that waits for an inbound `--connect` peer to reconnect.
///
/// Unlike `OutboundDialRetryWorker`, this worker does not actively dial the
/// peer. It only waits until either the peer reconnects (signalled by a
/// `RemoteNodeOnline` event, which cancels this worker) or the timeout expires.
///
/// Progress is reported back through `StateEvent`:
/// * `RemoteNodeReconnectFailed` when the timeout expires.
pub(crate) struct InboundConnectWaitWorker {
    pub(crate) cancel_tx: mpsc::Sender<()>,
}

impl InboundConnectWaitWorker {
    /// Start a worker that waits up to `wait_timeout` for the peer to reconnect.
    pub(crate) fn start(
        node_id: String,
        wait_timeout: Duration,
        state_tx: mpsc::Sender<StateEvent>,
    ) -> Self {
        let (cancel_tx, cancel_rx) = mpsc::channel::<()>();
        thread::spawn(move || {
            run_wait_loop(node_id, wait_timeout, state_tx, cancel_rx);
        });
        Self { cancel_tx }
    }
}

fn run_wait_loop(
    node_id: String,
    wait_timeout: Duration,
    state_tx: mpsc::Sender<StateEvent>,
    cancel_rx: mpsc::Receiver<()>,
) {
    ERROR_LOG.log(format!(
        "[inbound-connect-wait] {node_id} waiting up to {wait_timeout:?} for reconnect"
    ));

    let wait_started = Instant::now();
    match cancel_rx.recv_timeout(wait_timeout) {
        Ok(()) => {
            ERROR_LOG.log(format!(
                "[inbound-connect-wait] {node_id} cancelled after {:?}",
                wait_started.elapsed()
            ));
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            ERROR_LOG.log(format!(
                "[inbound-connect-wait] {node_id} cancel channel closed after {:?}",
                wait_started.elapsed()
            ));
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            ERROR_LOG.log(format!(
                "[inbound-connect-wait] {node_id} timeout after {wait_timeout:?}"
            ));
            let _ = state_tx.send(StateEvent::RemoteNodeReconnectFailed {
                node_id: node_id.clone(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn wait_worker_emits_reconnect_failed_on_timeout() {
        let (state_tx, state_rx) = mpsc::channel::<StateEvent>();
        let _worker = InboundConnectWaitWorker::start(
            "test-node".to_string(),
            Duration::from_millis(50),
            state_tx,
        );

        let event = state_rx
            .recv_timeout(Duration::from_millis(500))
            .expect("timeout event should arrive");
        match event {
            StateEvent::RemoteNodeReconnectFailed { node_id } => {
                assert_eq!(node_id, "test-node");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn wait_worker_does_not_emit_event_when_cancelled() {
        let (state_tx, state_rx) = mpsc::channel::<StateEvent>();
        let worker = InboundConnectWaitWorker::start(
            "test-node".to_string(),
            Duration::from_secs(60),
            state_tx,
        );

        let _ = worker.cancel_tx.send(());

        // Give the worker a moment to exit.
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            state_rx.try_recv().is_err(),
            "cancelled worker should not emit RemoteNodeReconnectFailed"
        );
    }
}
