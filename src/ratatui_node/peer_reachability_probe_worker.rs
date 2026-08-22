use crate::infra::error_log::ERROR_LOG;
use crate::ratatui_node::state_event::StateEvent;
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

/// How often to probe an outbound-dial peer while it is offline.
const PROBE_INTERVAL: Duration = Duration::from_secs(2);
/// How long to wait for a single TCP connect attempt.
const PROBE_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// Background worker that probes an outbound-dial peer at the L4 layer.
///
/// Unlike `NetworkProbe`, this checks the actual remote host:port, so it can
/// detect when a LAN peer recovers from a transient outage.  It only sends a
/// `RemoteNodeReachable` event on the transition from unreachable to reachable,
/// avoiding event spam while the peer stays reachable.
pub(crate) struct PeerReachabilityProbeWorker {
    pub(crate) cancel_tx: mpsc::Sender<()>,
}

impl PeerReachabilityProbeWorker {
    pub(crate) fn start(
        node_id: String,
        host: String,
        port: u16,
        state_tx: mpsc::Sender<StateEvent>,
    ) -> Self {
        let (cancel_tx, cancel_rx) = mpsc::channel::<()>();
        thread::spawn(move || {
            run_probe_loop(node_id, host, port, state_tx, cancel_rx);
        });
        Self { cancel_tx }
    }
}

fn run_probe_loop(
    node_id: String,
    host: String,
    port: u16,
    state_tx: mpsc::Sender<StateEvent>,
    cancel_rx: mpsc::Receiver<()>,
) {
    let mut was_reachable = false;
    let probe_started = Instant::now();

    loop {
        match cancel_rx.recv_timeout(PROBE_INTERVAL) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                ERROR_LOG.log(format!(
                    "[peer-reachability-probe] {node_id} cancelled after {:?}",
                    probe_started.elapsed()
                ));
                return;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }

        let now_reachable = probe_tcp_reachable(&host, port);

        if now_reachable && !was_reachable {
            ERROR_LOG.log(format!(
                "[peer-reachability-probe] {node_id} became reachable after {:?}",
                probe_started.elapsed()
            ));
            let _ = state_tx.send(StateEvent::RemoteNodeReachable {
                node_id: node_id.clone(),
            });
            was_reachable = true;
        } else if !now_reachable && was_reachable {
            ERROR_LOG.log(format!(
                "[peer-reachability-probe] {node_id} became unreachable again"
            ));
            was_reachable = false;
        }
    }
}

fn probe_tcp_reachable(host: &str, port: u16) -> bool {
    let addrs = match format!("{host}:{port}").to_socket_addrs() {
        Ok(iter) => iter.collect::<Vec<_>>(),
        Err(error) => {
            ERROR_LOG.log(format!(
                "[peer-reachability-probe] failed to resolve {host}:{port}: {error}"
            ));
            return false;
        }
    };

    for addr in addrs {
        if TcpStream::connect_timeout(&addr, PROBE_CONNECT_TIMEOUT).is_ok() {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn probe_worker_sends_reachable_event_on_transition() {
        // Bind a local TCP socket so the probe sees the peer as reachable.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        let (state_tx, state_rx) = mpsc::channel::<StateEvent>();
        let worker = PeerReachabilityProbeWorker::start(
            "test-node".to_string(),
            "127.0.0.1".to_string(),
            port,
            state_tx,
        );

        let event = state_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("reachable event should arrive");
        match event {
            StateEvent::RemoteNodeReachable { node_id } => {
                assert_eq!(node_id, "test-node");
            }
            other => panic!("unexpected event: {other:?}"),
        }

        let _ = worker.cancel_tx.send(());
    }

    #[test]
    fn probe_worker_does_not_emit_event_when_cancelled() {
        // Use a port where nothing is listening; the probe should stay silent.
        let (state_tx, state_rx) = mpsc::channel::<StateEvent>();
        let worker = PeerReachabilityProbeWorker::start(
            "test-node".to_string(),
            "127.0.0.1".to_string(),
            1,
            state_tx,
        );

        let _ = worker.cancel_tx.send(());
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            state_rx.try_recv().is_err(),
            "cancelled worker should not emit any event"
        );
    }
}
