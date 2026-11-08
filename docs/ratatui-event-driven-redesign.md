# WaitAgent Ratatui Server Event-Driven Redesign

Version: `v2.0`  
Status: `Draft`  
Date: `2026-07-30`

## 1. Purpose

The current `waitagent --ratatui` server process (`__ratatui-node-server`) uses a hybrid model internally:

- `RatatuiLocalSession` relies on `alacritty_terminal::EventLoop` and runs self-contained;
- `RatatuiAuthorityHostSession` hand-rolls PTY reader / writer / child-waiter threads, with three independent threads per authority-host session;
- a new watcher thread polls `authority_host_sessions` to detect exits;
- `SharedState` is mutated directly by multiple threads; lifecycle events of authority-host sessions travel over an `mpsc::Receiver` single-consumer channel, so the output pump and the watcher compete as two consumers for exit events.

This document defines the event-driven redesign of the server internals:

- `RatatuiLocalSession` stays intact and remains driven by `alacritty_terminal::EventLoop`;
- `RatatuiAuthorityHostSession` is instead driven by a unified `AuthorityHostIoLoop` and no longer spawns its own threads;
- a new `StateEventLoop` is added as the sole writer of `SharedState`;
- all lifecycle events (local child exit, authority-host child exit, client commands, session sync requests) converge on `StateEventLoop`;
- the polling watcher is removed;
- data flow is separated from control flow.

## 2. Scope

This redesign focuses on the internal architecture of the server process. The TUI client side keeps the existing event model:

- the client talks to the server over a Unix socket;
- the client has its own UI event loop (crossterm + ratatui);
- the client updates its UI from the server's `Snapshot` / `Disconnected` messages.

## 3. Non-Negotiable Principles

1. **No per-authority-host-session thread**. All authority-host PTY I/O is handled uniformly by `AuthorityHostIoLoop`.
2. **Child exit is no longer polled inside the session with `try_wait()`**. Local sessions are detected by `alacritty_terminal::EventLoop`; authority-host sessions are detected by the unified child waiter.
3. **All `SharedState` writes are performed only by `StateEventLoop`**. Other components read only, or request changes via events.
4. **Data flow does not enter `StateEventLoop`**. Raw authority-host PTY bytes are forwarded directly by `AuthorityHostIoLoop`; local session output is written directly into `Term` by `alacritty_terminal::EventLoop`.
5. **Remove the polling watcher**. Exits, resizes, title changes, etc. are all propagated via events.

## 4. Root Causes of Current Problems

### 4.1 authority-host session exit not synchronized

`RatatuiAuthorityHostSession` hand-rolls three threads:

- `spawn_pty_reader`: reads PTY output and hands it to the output pump via `output_rx`;
- `spawn_pty_writer`: reads input from `input_rx` and writes it to the PTY;
- `spawn_child_waiter`: polls `child.try_wait()` and, on exit, sends the exit code once to `exit_rx`.

The output pump in `local_session_sync_backends.rs:1265` calls `output_session.try_recv_exit()`, consuming the single exit code. As a result, the watcher's subsequent `try_recv_exit()` always returns `None`; the session is never removed from `SharedState`, and `TargetExited` is never published to the authority.

### 4.2 SharedState mutated directly

`RatatuiLocalTargetFactory::create_local_target` directly `insert`s into `sessions` and `authority_host_sessions`; client handlers call `feed_input` and `resize` directly. These writes are scattered across threads, making consistency hard to guarantee.

## 5. Target Architecture

```text
Server Process (waitagent __ratatui-node-server)
┌─────────────────────────────────────────────────────────────────────┐
│  Local Sessions                                                     │
│  - one alacritty_terminal::EventLoop per local session              │
│  - internally manages its own PTY I/O, Term updates, child exit     │
│  - EventProxy sends ChildExit / Title events to StateEventLoop      │
├─────────────────────────────────────────────────────────────────────┤
│  AuthorityHostIoLoop  (single thread, polling::Poller)              │
│                                                                     │
│  registers and polls:                                               │
│  - all authority-host PTY master fds                                │
│                                                                     │
│  behavior:                                                          │
│  - PTY readable → read raw bytes → forward to remote viewer         │
│  - input request received → write PTY master                        │
│  - resize request received → ioctl TIOCSWINSZ                       │
│  - send ChildExited / PtyClosed to StateEventLoop                   │
├─────────────────────────────────────────────────────────────────────┤
│  AuthorityHostIoLoop internals                                      │
│  - listen for SIGCHLD with signal-hook                              │
│  - on SIGCHLD, iterate authority-host children and call try_wait    │
│  - send StateEvent::AuthorityHostSessionChildExited                 │
│  - local children still detected by alacritty_terminal::EventLoop   │
├─────────────────────────────────────────────────────────────────────┤
│  Tokio Runtime                                                      │
│  - client Unix sockets                                              │
│  - remote gRPC streams (session sync / ingress / remote viewer)     │
│  - turn client commands / sync requests into StateEventLoop events  │
├─────────────────────────────────────────────────────────────────────┤
│  StateEventLoop  (single thread)                                    │
│  - update SharedState                                               │
│  - call handle_session_exit / activate_target / create_*            │
│  - send LocalCatalogChangeRequest to session sync runtime           │
│  - broadcast Snapshot to all clients                                │
└─────────────────────────────────────────────────────────────────────┘
                                    │
                                    │ Unix socket
                                    ▼
┌─────────────────────────────────────────────────────────────────────┐
│  TUI Client Process (waitagent --ratatui)                           │
│  UI Event Loop (crossterm + ratatui)                                │
│  - receive Snapshot → render sidebar / main pane / footer           │
│  - send ClientMessage → server                                      │
└─────────────────────────────────────────────────────────────────────┘
```

## 6. Why This Split

### Local sessions stay intact

A local session is a full tty:

- it needs a terminal emulator to maintain the screen buffer;
- its output is only for the local TUI;
- `alacritty_terminal::EventLoop` already handles all of this, and is mature.

Forcing it apart into "its own I/O Loop + Term" would only add complexity with no clear benefit.

### Authority-host sessions must be split apart

An authority-host session is not a tty display; it is a **raw byte forwarder**:

- no terminal emulator is needed;
- output must be forwarded to the remote viewer unaltered;
- the current hand-rolled threads make exit detection unreliable.

So a dedicated I/O loop is needed to manage them uniformly.

### StateEventLoop unifies state

Whether local or authority-host, session create / exit / activate / resize all end up mutating the same `SharedState`. Converging these on a single-threaded event loop avoids lock contention and inconsistent state.

## 7. Event Types

### 7.1 Events entering StateEventLoop

```rust
enum StateEvent {
    // --- local session lifecycle (from alacritty_terminal EventProxy) ---
    LocalSessionChildExit {
        session_id: String,
        exit_code: i32,
    },
    LocalSessionTitleChanged {
        session_id: String,
        title: String,
    },

    // --- authority-host session lifecycle (from AuthorityHostIoLoop / child waiter) ---
    AuthorityHostSessionChildExited {
        session_id: String,
        exit_code: i32,
    },
    AuthorityHostSessionPtyClosed {
        session_id: String,
    },

    // --- client (from tokio client tasks) ---
    ClientConnected {
        client_id: usize,
    },
    ClientDisconnected {
        client_id: usize,
    },
    ClientInput {
        session_id: String,
        bytes: Vec<u8>,
    },
    ClientActivatedTarget {
        target_id: String,
    },
    ClientResized {
        cols: u16,
        rows: u16,
    },
    ClientCreateLocalSession,

    // --- session sync / remote ---
    CreateAuthorityHostSession {
        request_id: String,
        cols: u16,
        rows: u16,
    },
    RemoteSessionClosed {
        target_id: String,
    },
}
```

### 7.2 AuthorityHostIoLoop internal messages

```rust
enum AuthorityHostIoRequest {
    WriteInput {
        session_id: String,
        bytes: Vec<u8>,
    },
    Resize {
        session_id: String,
        cols: u16,
        rows: u16,
    },
    RegisterSession {
        session_id: String,
        pty_master: File,
    },
    UnregisterSession {
        session_id: String,
    },
}
```

## 8. Local Session Redesign

### 8.1 Keep `alacritty_terminal::EventLoop`

```rust
pub struct RatatuiLocalSession {
    pub session_id: String,
    pub command_name: String,
    pub term: Arc<FairMutex<Term<EventProxy>>>,
    event_loop_sender: Arc<Mutex<Option<EventLoopSender>>>,
}
```

### 8.2 EventProxy only sends events to StateEventLoop

```rust
impl EventListener for EventProxy {
    fn send_event(&self, event: Event) {
        match event {
            Event::ChildExit(status) => {
                self.state_tx.send(StateEvent::LocalSessionChildExit {
                    session_id: self.session_id.clone(),
                    exit_code: status,
                });
            }
            Event::Exit => {
                self.state_tx.send(StateEvent::LocalSessionChildExit {
                    session_id: self.session_id.clone(),
                    exit_code: -1,
                });
            }
            Event::Title(title) => {
                self.state_tx.send(StateEvent::LocalSessionTitleChanged {
                    session_id: self.session_id.clone(),
                    title,
                });
            }
            // Wakeup no longer drives snapshots; StateEventLoop broadcasts automatically after output events
            _ => {}
        }
    }
}
```

### 8.3 feed_input / resize / shutdown

These methods still send `Msg::Input` / `Msg::Resize` / `Msg::Shutdown` to `alacritty_terminal::EventLoop` via `EventLoopSender`. No changes needed.

## 9. AuthorityHostIoLoop

### 9.1 Registering the PTY fd

Use the `polling` crate (already pulled in via `alacritty_terminal`):

```rust
let mut poller = polling::Poller::new()?;

for (session_id, pty_master) in &authority_host_sessions {
    poller.add(
        pty_master,
        polling::Event::readable(session_id_to_token(session_id)),
        polling::PollMode::Level,
    )?;
}
```

### 9.2 Read and forward

```rust
loop {
    poller.wait(&mut events, None)?;
    for event in events.iter() {
        let session_id = token_to_session_id(event.key);
        let session = authority_host_sessions.get(&session_id);

        if event.readable {
            match read_pty_nonblocking(&session.pty_master, &mut buf) {
                Ok(0) => {
                    state_tx.send(StateEvent::AuthorityHostSessionPtyClosed { session_id });
                }
                Ok(n) => {
                    forward_to_remote_viewer(&session_id, &buf[..n]);
                }
                Err(_) => {
                    state_tx.send(StateEvent::AuthorityHostSessionPtyClosed { session_id });
                }
            }
        }
    }
}
```

`forward_to_remote_viewer` sends `RawPtyOutput` over the session sync active gRPC session.

### 9.3 Writing input / Resize

```rust
// internal channel receives AuthorityHostIoRequest
while let Ok(req) = io_rx.try_recv() {
    match req {
        AuthorityHostIoRequest::WriteInput { session_id, bytes } => {
            write_pty_nonblocking(&sessions[&session_id].pty_master, &bytes);
        }
        AuthorityHostIoRequest::Resize { session_id, cols, rows } => {
            ioctl_tiocswinsz(&sessions[&session_id].pty_master, cols, rows);
        }
        AuthorityHostIoRequest::RegisterSession { session_id, pty_master } => {
            poller.add(...)?;
        }
        AuthorityHostIoRequest::UnregisterSession { session_id } => {
            poller.delete(...)?;
        }
    }
}
```

## 10. Authority-Host Child Exit Detection

Child exit for authority-host sessions is detected uniformly by `AuthorityHostIoLoop`; no separate waiter thread is spawned.

### 10.1 Listening for SIGCHLD with signal-hook

The `signal_hook` crate is already available via `alacritty_terminal`. Register SIGCHLD on a pipe:

```rust
use signal_hook::consts::signal::SIGCHLD;
use signal_hook::low_level::pipe::register;

let (sig_read, sig_write) = UnixStream::pair()?;
register(SIGCHLD, sig_write)?;

poller.add(
    &sig_read,
    polling::Event::readable(SIGCHLD_TOKEN),
    polling::PollMode::Level,
)?;
```

### 10.2 Handling it in AuthorityHostIoLoop

```rust
loop {
    poller.wait(&mut events, None)?;

    for event in events.iter() {
        if event.key == SIGCHLD_TOKEN {
            // drain the signal pipe
            let mut buf = [0u8; 1];
            let _ = sig_read.read(&mut buf);

            // iterate all authority-host children, checking exit with WNOHANG
            for (session_id, child) in &mut authority_host_children {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        state_tx.send(StateEvent::AuthorityHostSessionChildExited {
                            session_id: session_id.clone(),
                            exit_code: status.code().unwrap_or(-1),
                        });
                    }
                    Ok(None) => {}
                    Err(err) => {
                        error!("try_wait failed for {session_id}: {err}");
                    }
                }
            }
        }

        // ... PTY readable handling
    }
}
```

### 10.3 No conflict with local sessions

- Local session children are detected by `alacritty_terminal::EventLoop`'s own SIGCHLD handler;
- authority-host session children are detected by `AuthorityHostIoLoop`'s SIGCHLD handler;
- each side calls `try_wait()` only on the children it manages, so they do not compete.

Under this design there is no dedicated child waiter thread; child exit events are driven entirely by the `AuthorityHostIoLoop` event loop.

## 11. StateEventLoop

```rust
async fn run_state_event_loop(
    shared: Arc<SharedState>,
    mut state_rx: Receiver<StateEvent>,
    catalog_tx: Sender<LocalCatalogChangeRequest>,
    authority_host_io_tx: Sender<AuthorityHostIoRequest>,
) {
    while let Some(event) = state_rx.recv().await {
        match event {
            StateEvent::LocalSessionChildExit { session_id, .. }
            | StateEvent::AuthorityHostSessionChildExited { session_id, .. }
            | StateEvent::AuthorityHostSessionPtyClosed { session_id } => {
                shared.handle_session_exit(&session_id);
                notify_catalog_changed(&catalog_tx, LocalCatalogChangeReason::LocalTargetExited {
                    target_session_name: session_id,
                });
                let _ = shared.broadcast_snapshot();
            }

            StateEvent::LocalSessionTitleChanged { session_id, title } => {
                shared.set_local_session_title(&session_id, title);
                let _ = shared.broadcast_snapshot();
            }

            StateEvent::ClientConnected { client_id } => {
                shared.add_client(client_id);
                let _ = shared.broadcast_snapshot();
            }

            StateEvent::ClientDisconnected { client_id } => {
                shared.remove_client(client_id);
                // if this is the last client and there are no sessions, we can optionally shut down
            }

            StateEvent::ClientInput { session_id, bytes } => {
                if is_local_session(&session_id) {
                    shared.feed_local_session_input(&session_id, bytes);
                } else {
                    authority_host_io_tx.send(AuthorityHostIoRequest::WriteInput {
                        session_id,
                        bytes,
                    });
                }
            }

            StateEvent::ClientActivatedTarget { target_id } => {
                shared.activate_target(&target_id);
                let _ = shared.broadcast_snapshot();
            }

            StateEvent::ClientResized { cols, rows } => {
                shared.resize_active_sessions(cols, rows);
                // the active authority-host session must be notified via AuthorityHostIoLoop
                if let Some(id) = active_authority_host_session(&shared) {
                    authority_host_io_tx.send(AuthorityHostIoRequest::Resize {
                        session_id: id,
                        cols,
                        rows,
                    });
                }
            }

            StateEvent::ClientCreateLocalSession => {
                let target = shared.create_local_session(next_id(), 80, 24);
                notify_catalog_changed(&catalog_tx, LocalCatalogChangeReason::LocalRuntimeChanged);
                let _ = shared.broadcast_snapshot();
            }

            StateEvent::CreateAuthorityHostSession { request_id, cols, rows } => {
                let session_id = shared.create_authority_host_session(cols, rows);
                authority_host_io_tx.send(AuthorityHostIoRequest::RegisterSession {
                    session_id: session_id.clone(),
                    pty_master: /* fd */,
                });
                notify_catalog_changed(&catalog_tx, LocalCatalogChangeReason::LocalRuntimeChanged);
                let _ = shared.broadcast_snapshot();
                // reply to session sync with CreateSessionAccepted
            }

            StateEvent::RemoteSessionClosed { target_id } => {
                shared.handle_remote_viewer_closed(&target_id);
                let _ = shared.broadcast_snapshot();
            }
        }
    }
}
```

## 12. Session Sync Runtime Interaction

### 12.1 Creating an authority-host session

```text
remote authority
    │
    │ CreateSessionRequest
    ▼
session sync runtime task (tokio)
    │
    │ does not create directly; sends StateEvent::CreateAuthorityHostSession
    ▼
StateEventLoop
    │
    │ creates RatatuiAuthorityHostSession, registers it with AuthorityHostIoLoop
    │ sends LocalCatalogChangeRequest
    ▼
session sync runtime
    │
    │ publishes TargetPublished to the authority
    ▼
local server updates the sidebar
```

### 12.2 authority-host session exit

```text
shell exit
    │
    │ AuthorityHostIoLoop SIGCHLD handler
    ▼
StateEventLoop
    │
    │ removes it from SharedState, sends LocalCatalogChangeRequest
    ▼
session sync runtime
    │
    │ publishes TargetExited to the authority
    ▼
local server receives TargetExited
    │
    │ signal_remote_target_exited -> StateEvent::RemoteSessionClosed
    ▼
StateEventLoop updates local catalog, broadcasts snapshot
```

## 13. Client Communication

Client Unix sockets are handled by the tokio runtime:

```rust
tokio::spawn(async move {
    let listener = tokio::net::UnixListener::bind(socket_path)?;
    loop {
        let (stream, _) = listener.accept().await?;
        tokio::spawn(handle_client(stream, state_tx.clone()));
    }
});
```

`handle_client`:

- connection established → `StateEvent::ClientConnected`
- message read → converted to the corresponding `StateEvent`
- snapshot received from StateEventLoop → written to the socket
- disconnect → `StateEvent::ClientDisconnected`

## 14. Implementation Phases

### Phase 1: StateEventLoop + AuthorityHostIoLoop

- add `StateEventLoop`;
- add `AuthorityHostIoLoop`, based on the `polling` crate;
- rewrite `RatatuiAuthorityHostSession`: keep only fd + pid, spawn no threads;
- listen for SIGCHLD with signal-hook in `AuthorityHostIoLoop` to detect authority-host child exit;
- change `RatatuiLocalTargetFactory::create_local_target` to send events to `StateEventLoop`;
- verify authority-host session exit propagates correctly.

### Phase 2: Local Session integration with StateEventLoop

- `EventProxy` no longer mutates `SharedState` directly; it sends `StateEvent`s instead;
- `handle_session_exit`  moved into `StateEventLoop`;
- remove `LocalSessionEvent` and the separate worker thread;
- verify local session exit behavior is unchanged.

### Phase 3: Client Handler integration with StateEventLoop

- client sockets handled by tokio;
- client commands converted into `StateEvent`s;
- snapshot broadcast writes to sockets via tokio client tasks;
- remove per-client `std::thread::spawn`.

### Phase 4: SharedState lock removal

- since all writes happen in `StateEventLoop`, gradually remove the `SharedState` internal Mutex;
- clients take a read-only reference or a clone when reading snapshots.

## 15. Risks and Notes

1. **SIGCHLD does not conflict with `alacritty_terminal`**  
   `alacritty_terminal::EventLoop` registers its own SIGCHLD handler; `AuthorityHostIoLoop` also registers a SIGCHLD handler. On Linux, multiple signalfd/pipe registrations can receive SIGCHLD at the same time. After the signal arrives, each side only iterates the children it manages and calls `try_wait()`, so they do not compete.

2. **Coexistence of the `polling` crate with tokio**  
   gRPC / session sync remain tokio async. `AuthorityHostIoLoop` is a synchronous thread + `polling`. The two communicate via channels, with a clear boundary.

3. **Client socket write blocking**  
   Snapshot broadcast must not block `StateEventLoop` directly. Send the snapshot to client tasks over a tokio channel, and let the client tasks write to the socket asynchronously.

4. **PTY output batching**  
   `AuthorityHostIoLoop` should read PTY output in batches, to avoid a system call for every byte.

## 16. Acceptance Criteria

- after `waitagent --ratatui` starts, authority-host sessions no longer spawn a reader/writer/child-waiter thread per session;
- typing `exit` in a remote session, the remote node detects the exit within 100ms and publishes `TargetExited`;
- the local sidebar item disappears and focus switches automatically;
- typing `exit` in a local session, the server shuts down correctly or switches to the next session;
- `cargo test --release ratatui session_sync` passes fully.
