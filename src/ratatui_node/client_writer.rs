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
    Register { client_id: u64, stream: LocalStream },
    /// Write a single line payload to one client.
    Write { client_id: u64, payload: String },
    /// Write a single line payload to every registered client.
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

/// Single thread that owns all TUI client socket writes.
pub(crate) struct ClientWriter;

impl ClientWriter {
    pub(crate) fn start() -> ClientWriterHandle {
        let (tx, rx) = mpsc::channel::<ClientWriterRequest>();
        std::thread::spawn(move || Self::run(rx));
        ClientWriterHandle { tx }
    }

    fn run(rx: Receiver<ClientWriterRequest>) {
        let mut clients: HashMap<u64, LocalStream> = HashMap::new();
        while let Ok(request) = rx.recv() {
            match request {
                ClientWriterRequest::Register { client_id, stream } => {
                    clients.insert(client_id, stream);
                }
                ClientWriterRequest::Write { client_id, payload } => {
                    if let Some(stream) = clients.get_mut(&client_id) {
                        if writeln!(stream, "{payload}").is_err() || stream.flush().is_err() {
                            ERROR_LOG.log(format!(
                                "[ratatui-node] failed to write to client {client_id}; removing"
                            ));
                            let _ = clients.remove(&client_id);
                        }
                    }
                }
                ClientWriterRequest::Broadcast { payload } => {
                    let mut failed = Vec::new();
                    for (client_id, stream) in clients.iter_mut() {
                        if writeln!(stream, "{payload}").is_err() || stream.flush().is_err() {
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
                    if let Some(stream) = clients.remove(&client_id) {
                        let _ = stream.shutdown(std::net::Shutdown::Both);
                    }
                }
            }
        }
    }
}
