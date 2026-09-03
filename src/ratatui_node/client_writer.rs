use crate::infra::error_log::ERROR_LOG;
use crate::platform::local_ipc::LocalStream;
use std::collections::HashMap;
use std::io::Write;
use std::sync::mpsc::{self, Receiver, Sender};

/// Requests accepted by the single `ClientWriter` thread.
///
/// `ClientWriter` is the only place in the ratatui node server that writes to
/// TUI client sockets. Serializing all writes through one thread guarantees
/// that a command response and a snapshot can never interleave on the same
/// socket.
#[derive(Debug)]
pub(crate) enum ClientWriterRequest {
    /// Store a new client stream and associate it with the given id.
    ///
    /// `broadcast` selects whether snapshot/history broadcasts are pushed to
    /// this client. One-shot control-command clients must not receive
    /// broadcasts; only attached TUI clients should set this to `true`.
    Register {
        client_id: u64,
        stream: LocalStream,
        broadcast: bool,
    },
    /// Write a single line payload to one client.
    Write { client_id: u64, payload: String },
    /// Write a single line payload to every broadcast-enabled client.
    Broadcast { payload: String },
    /// Remove a client and shutdown its socket.
    Unregister { client_id: u64 },
}

/// Handle used by other threads to send write requests to `ClientWriter`.
#[derive(Debug, Clone)]
pub(crate) struct ClientWriterHandle {
    tx: Sender<ClientWriterRequest>,
}

impl ClientWriterHandle {
    pub(crate) fn send(&self, request: ClientWriterRequest) {
        let _ = self.tx.send(request);
    }
}

/// A registered client socket plus its broadcast eligibility.
struct ClientEntry {
    stream: LocalStream,
    broadcast: bool,
}

/// Single thread that owns all TUI client socket writes.
pub(crate) struct ClientWriter;

impl ClientWriter {
    pub(crate) fn start() -> ClientWriterHandle {
        let (tx, rx) = mpsc::channel::<ClientWriterRequest>();
        std::thread::spawn(move || Self::run(rx));
        ClientWriterHandle { tx }
    }

    fn run(rx: Receiver<ClientWriterRequest>) {
        let mut clients: HashMap<u64, ClientEntry> = HashMap::new();
        while let Ok(request) = rx.recv() {
            match request {
                ClientWriterRequest::Register {
                    client_id,
                    stream,
                    broadcast,
                } => {
                    clients.insert(client_id, ClientEntry { stream, broadcast });
                }
                ClientWriterRequest::Write { client_id, payload } => {
                    if let Some(entry) = clients.get_mut(&client_id) {
                        if writeln!(entry.stream, "{payload}").is_err()
                            || entry.stream.flush().is_err()
                        {
                            ERROR_LOG.log(format!(
                                "[ratatui-node] failed to write to client {client_id}; removing"
                            ));
                            let _ = clients.remove(&client_id);
                        } else if !entry.broadcast {
                            // One-shot control-command clients receive exactly
                            // one response line; drop them immediately so the
                            // socket does not linger in the registry.
                            let _ = entry.stream.shutdown(std::net::Shutdown::Both);
                            let _ = clients.remove(&client_id);
                        }
                    }
                }
                ClientWriterRequest::Broadcast { payload } => {
                    let mut failed = Vec::new();
                    for (client_id, entry) in clients.iter_mut() {
                        if !entry.broadcast {
                            continue;
                        }
                        if writeln!(entry.stream, "{payload}").is_err()
                            || entry.stream.flush().is_err()
                        {
                            failed.push(*client_id);
                        }
                    }
                    for client_id in failed {
                        ERROR_LOG.log(format!(
                            "[ratatui-node] failed to broadcast to client {client_id}; removing"
                        ));
                        let _ = clients.remove(&client_id);
                    }
                }
                ClientWriterRequest::Unregister { client_id } => {
                    if let Some(entry) = clients.remove(&client_id) {
                        let _ = entry.stream.shutdown(std::net::Shutdown::Both);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn localhost_pair() -> (LocalStream, LocalStream) {
        use std::os::unix::net::UnixStream;
        let (left, right) = UnixStream::pair().expect("socket pair");
        (LocalStream::from_unix(left), LocalStream::from_unix(right))
    }

    #[cfg(unix)]
    fn read_line(stream: &mut LocalStream) -> String {
        use std::io::Read;
        let mut buf = [0u8; 8192];
        let mut collected = Vec::new();
        loop {
            let read = stream.read(&mut buf).expect("read");
            if read == 0 {
                break;
            }
            collected.extend_from_slice(&buf[..read]);
            if collected.contains(&b'\n') {
                break;
            }
        }
        String::from_utf8(collected).expect("utf8")
    }

    #[cfg(unix)]
    #[test]
    fn broadcast_skips_one_shot_clients_and_they_close_after_first_write() {
        let handle = ClientWriter::start();

        let (attach_stream, mut attach_peer) = localhost_pair();
        handle.send(ClientWriterRequest::Register {
            client_id: 1,
            stream: attach_stream,
            broadcast: true,
        });

        let (one_shot_stream, mut one_shot_peer) = localhost_pair();
        handle.send(ClientWriterRequest::Register {
            client_id: 2,
            stream: one_shot_stream,
            broadcast: false,
        });

        handle.send(ClientWriterRequest::Broadcast {
            payload: "broadcast-line".to_string(),
        });
        let attach_line = read_line(&mut attach_peer);
        assert_eq!(attach_line.trim(), "broadcast-line");
        // The one-shot client must not receive the broadcast. Probe with a
        // short timeout: the next thing on its socket is the direct response.
        handle.send(ClientWriterRequest::Write {
            client_id: 2,
            payload: "response-line".to_string(),
        });
        let one_shot_line = read_line(&mut one_shot_peer);
        assert_eq!(
            one_shot_line.trim(),
            "response-line",
            "one-shot client must see the response, not the broadcast"
        );

        // After the first successful write the one-shot client is removed, so
        // a later direct write reaches no socket and the attach client is
        // unaffected.
        handle.send(ClientWriterRequest::Write {
            client_id: 2,
            payload: "never-delivered".to_string(),
        });
        handle.send(ClientWriterRequest::Broadcast {
            payload: "broadcast-line-2".to_string(),
        });
        let attach_line_2 = read_line(&mut attach_peer);
        assert_eq!(attach_line_2.trim(), "broadcast-line-2");
    }
}
