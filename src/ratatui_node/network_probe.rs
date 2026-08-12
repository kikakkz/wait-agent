use crate::infra::error_log::ERROR_LOG;
use crate::ratatui_node::state_event::StateEvent;
use std::net::{SocketAddr, UdpSocket};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

/// Default endpoints used to probe whether the control plane has upstream
/// connectivity.  UDP port 53 is used so the probe resembles a DNS packet but
/// does not actually need a response; `connect()` alone tells us whether the
/// local routing stack can reach the target.
const DEFAULT_PROBE_TARGETS: [([u8; 4], u16); 2] = [([8, 8, 8, 8], 53), ([1, 1, 1, 1], 53)];
const PROBE_INTERVAL: Duration = Duration::from_secs(5);

/// Background probe that monitors whether the control plane can reach the
/// public internet.
///
/// The probe runs in its own thread and sends `StateEvent::NetworkConnectivityChanged`
/// to the state loop only when connectivity transitions between online and
/// offline.  This lets `StateEventLoop` decide whether a remote host dropout
/// should be treated as a transient control-plane outage or a permanent remote
/// failure.
pub(crate) struct NetworkProbe {
    _stop_tx: mpsc::Sender<()>,
}

impl NetworkProbe {
    pub(crate) fn start(state_tx: mpsc::Sender<StateEvent>) -> Self {
        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        thread::spawn(move || run_probe_loop(state_tx, stop_rx));
        Self { _stop_tx: stop_tx }
    }
}

fn run_probe_loop(state_tx: mpsc::Sender<StateEvent>, stop_rx: mpsc::Receiver<()>) {
    let mut last_state: Option<bool> = None;
    let mut last_probe = Instant::now();

    loop {
        if stop_rx.try_recv().is_ok() {
            return;
        }

        if last_probe.elapsed() >= PROBE_INTERVAL {
            last_probe = Instant::now();
            let online = probe_connectivity();
            if last_state != Some(online) {
                ERROR_LOG.log(format!(
                    "[network-probe] connectivity changed online={online}"
                ));
                if state_tx
                    .send(StateEvent::NetworkConnectivityChanged { online })
                    .is_err()
                {
                    return;
                }
                last_state = Some(online);
            }
        }

        // Short sleep so we respond promptly to stop while still spacing probes.
        match stop_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(()) => return,
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
}

fn probe_connectivity() -> bool {
    for (ip, port) in DEFAULT_PROBE_TARGETS {
        if can_reach(SocketAddr::from((ip, port))) {
            return true;
        }
    }
    false
}

fn can_reach(target: SocketAddr) -> bool {
    let Ok(socket) = UdpSocket::bind(SocketAddr::from(([0, 0, 0, 0], 0))) else {
        return false;
    };
    // UDP connect returns an error only if the local stack cannot route to the
    // target.  No packet needs to be sent.
    socket.connect(target).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    #[test]
    fn probe_can_reach_public_dns() {
        // This test depends on network access.  It is expected to pass on most
        // development machines; if run in an isolated environment it may fail.
        assert!(can_reach(SocketAddr::from(([8, 8, 8, 8], 53))));
    }
}
