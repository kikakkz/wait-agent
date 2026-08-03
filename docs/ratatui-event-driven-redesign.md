# WaitAgent Ratatui Server Event-Driven Redesign

Version: `v2.0`  
Status: `Draft`  
Date: `2026-07-30`

## 1. Purpose

当前 `waitagent --ratatui` 的 server 进程（`__ratatui-node-server`）内部采用混合模型：

- `RatatuiLocalSession` 依赖 `alacritty_terminal::EventLoop`，自包含运行；
- `RatatuiAuthorityHostSession` 手写 PTY reader / writer / child-waiter，每个 authority-host session 有三个独立 thread；
- 新增 watcher thread 轮询 `authority_host_sessions` 来检测退出；
- `SharedState` 被多个 thread 直接修改，authority-host session 的生命周期事件靠 `mpsc::Receiver` 单消费者 channel 传递，导致 output pump 和 watcher 两个消费者竞争退出事件。

本文档定义 server 内部的 event-driven 改造方案：

- `RatatuiLocalSession` 保持完整，继续由 `alacritty_terminal::EventLoop` 驱动；
- `RatatuiAuthorityHostSession` 改为由统一的 `AuthorityHostIoLoop` 驱动，不再自己 spawn thread；
- 新增 `StateEventLoop`，作为 `SharedState` 的唯一写者；
- 所有生命周期事件（local child exit、authority-host child exit、client 命令、session sync 请求）都 converge 到 `StateEventLoop`；
- 移除 polling watcher；
- 数据流与控制流分离。

## 2. Scope

本次 redesign 聚焦 server 进程内部架构。TUI client 侧保持现有事件模型：

- client 通过 Unix socket 与 server 通信；
- client 有自己的 UI event loop（crossterm + ratatui）；
- client 接收 server 的 `Snapshot` / `Disconnected` 消息更新 UI。

## 3. 非协商原则

1. **没有 per-authority-host-session thread**。所有 authority-host PTY I/O 由 `AuthorityHostIoLoop` 统一处理。
2. **子进程退出不再由 session 内部轮询 `try_wait()`**。local session 由 `alacritty_terminal::EventLoop` 检测；authority-host session 由统一 child waiter 检测。
3. **`SharedState` 写操作只由 `StateEventLoop` 执行**。其他组件只读或通过事件请求修改。
4. **数据流不进 `StateEventLoop`**。authority-host 原始 PTY 字节由 `AuthorityHostIoLoop` 直接转发；local session 输出由 `alacritty_terminal::EventLoop` 直接写进 `Term`。
5. **移除 polling watcher**。退出、resize、title 变化等都通过事件传播。

## 4. 当前问题根因

### 4.1 authority-host session 退出未同步

`RatatuiAuthorityHostSession` 手写三个 thread：

- `spawn_pty_reader`：读 PTY 输出，经 `output_rx` 给 output pump；
- `spawn_pty_writer`：从 `input_rx` 读输入写 PTY；
- `spawn_child_waiter`：轮询 `child.try_wait()`，退出后发送一次 `exit_code` 到 `exit_rx`。

`local_session_sync_backends.rs:1265` 的 output pump 调用 `output_session.try_recv_exit()` 消费了唯一的退出码，导致后续 watcher 的 `try_recv_exit()` 永远返回 `None`，session 无法从 `SharedState` 移除，`TargetExited` 也不会发布给 authority。

### 4.2 SharedState 被直接修改

`RatatuiLocalTargetFactory::create_local_target` 直接 `insert` 到 `sessions` 和 `authority_host_sessions`；client handler 直接调用 `feed_input`、`resize`；这些写操作分散在各个 thread 中，难以保证一致性。

## 5. 目标架构

```text
Server Process (waitagent __ratatui-node-server)
┌─────────────────────────────────────────────────────────────────────┐
│  Local Sessions                                                     │
│  - 每个 local session 一个 alacritty_terminal::EventLoop            │
│  - 内部管理自己的 PTY I/O、Term 更新、child exit                    │
│  - EventProxy 把 ChildExit / Title 事件发给 StateEventLoop         │
├─────────────────────────────────────────────────────────────────────┤
│  AuthorityHostIoLoop  (single thread, polling::Poller)              │
│                                                                     │
│  注册并 poll：                                                       │
│  - 所有 authority-host PTY master fd                               │
│                                                                     │
│  行为：                                                              │
│  - PTY 可读 → 读取原始字节 → 转发给 remote viewer                   │
│  - 收到输入请求 → 写 PTY master                                     │
│  - 收到 resize 请求 → ioctl TIOCSWINSZ                              │
│  - 把 ChildExited / PtyClosed 发给 StateEventLoop                  │
├─────────────────────────────────────────────────────────────────────┤
│  AuthorityHostIoLoop 内部                                           │
│  - 用 signal-hook 监听 SIGCHLD                                     │
│  - SIGCHLD 触发时遍历 authority-host children 调用 try_wait         │
│  - 发送 StateEvent::AuthorityHostSessionChildExited                │
│  - local session 子进程仍由 alacritty_terminal::EventLoop 检测      │
├─────────────────────────────────────────────────────────────────────┤
│  Tokio Runtime                                                      │
│  - client Unix sockets                                              │
│  - remote gRPC streams (session sync / ingress / remote viewer)    │
│  - 把 client 命令 / session sync 请求转成事件发给 StateEventLoop   │
├─────────────────────────────────────────────────────────────────────┤
│  StateEventLoop  (single thread)                                    │
│  - 更新 SharedState                                                 │
│  - 调用 handle_session_exit / activate_target / create_*           │
│  - 发送 LocalCatalogChangeRequest 给 session sync runtime          │
│  - broadcast Snapshot 给所有 clients                               │
└─────────────────────────────────────────────────────────────────────┘
                                    │
                                    │ Unix socket
                                    ▼
┌─────────────────────────────────────────────────────────────────────┐
│  TUI Client Process (waitagent --ratatui)                           │
│  UI Event Loop (crossterm + ratatui)                                │
│  - 接收 Snapshot → 渲染 sidebar / main pane / footer               │
│  - 发送 ClientMessage → server                                     │
└─────────────────────────────────────────────────────────────────────┘
```

## 6. 为什么这样分

### Local session 保持完整

Local session 就是一个完整 tty：

- 需要 terminal emulator 维护 screen buffer；
- 输出只给本地 TUI 用；
- `alacritty_terminal::EventLoop` 已经成熟处理这些。

强行拆开成"自己的 I/O Loop + Term"只会增加复杂度，没有明显收益。

### Authority-host session 必须拆开

Authority-host session 不是 tty 显示，而是**原始字节转发器**：

- 不需要 terminal emulator；
- 输出要原封不动发给远端 viewer；
- 当前手写 thread 导致退出检测不可靠。

所以需要一个专门的 I/O loop 来统一管理。

### StateEventLoop 统一状态

无论 local 还是 authority-host，session 的 create / exit / activate / resize 最终都要修改同一份 `SharedState`。把这些收敛到一个单线程 event loop，可以避免锁竞争和状态不一致。

## 7. 事件类型

### 7.1 进入 StateEventLoop 的事件

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

### 7.2 AuthorityHostIoLoop 内部消息

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

## 8. Local Session 改造

### 8.1 保留 `alacritty_terminal::EventLoop`

```rust
pub struct RatatuiLocalSession {
    pub session_id: String,
    pub command_name: String,
    pub term: Arc<FairMutex<Term<EventProxy>>>,
    event_loop_sender: Arc<Mutex<Option<EventLoopSender>>>,
}
```

### 8.2 EventProxy 改为只发事件给 StateEventLoop

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
            // Wakeup 不再用于驱动 snapshot；StateEventLoop 在 output 事件后自动 broadcast
            _ => {}
        }
    }
}
```

### 8.3 feed_input / resize / shutdown

这些方法仍然通过 `EventLoopSender` 发送 `Msg::Input` / `Msg::Resize` / `Msg::Shutdown` 给 `alacritty_terminal::EventLoop`。不需要改动。

## 9. AuthorityHostIoLoop

### 9.1 注册 PTY fd

使用 `polling` crate（已通过 `alacritty_terminal` 引入）：

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

### 9.2 读取并转发

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

`forward_to_remote_viewer` 通过 session sync 的 active gRPC session 发送 `RawPtyOutput`。

### 9.3 写输入 / Resize

```rust
// 内部 channel 接收 AuthorityHostIoRequest
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

## 10. Authority-Host Child Exit 检测

Authority-host session 的子进程退出由 `AuthorityHostIoLoop` 统一检测，不单独 spawn waiter thread。

### 10.1 使用 signal-hook 监听 SIGCHLD

`signal_hook` crate 已通过 `alacritty_terminal` 引入。注册 SIGCHLD 到一个 pipe：

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

### 10.2 在 AuthorityHostIoLoop 中处理

```rust
loop {
    poller.wait(&mut events, None)?;

    for event in events.iter() {
        if event.key == SIGCHLD_TOKEN {
            // 清空 signal pipe
            let mut buf = [0u8; 1];
            let _ = sig_read.read(&mut buf);

            // 遍历所有 authority-host children，用 WNOHANG 检查退出
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

        // ... PTY 可读处理
    }
}
```

### 10.3 与 Local Session 不冲突

- Local session 子进程由 `alacritty_terminal::EventLoop` 自己的 SIGCHLD handler 检测；
- Authority-host session 子进程由 `AuthorityHostIoLoop` 的 SIGCHLD handler 检测；
- 两者各自调用自己管理的 child 的 `try_wait()`，互不竞争。

这种设计下没有专门的 child waiter thread，child exit 事件完全由 `AuthorityHostIoLoop` 的事件循环驱动。

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
                // 如果最后一个 client 且没有 session，可选择 shutdown
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
                // active authority-host session 需要通知 AuthorityHostIoLoop
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
                // 回复 session sync CreateSessionAccepted
            }

            StateEvent::RemoteSessionClosed { target_id } => {
                shared.handle_remote_viewer_closed(&target_id);
                let _ = shared.broadcast_snapshot();
            }
        }
    }
}
```

## 12. Session Sync Runtime 交互

### 12.1 创建 authority-host session

```text
remote authority
    │
    │ CreateSessionRequest
    ▼
session sync runtime task (tokio)
    │
    │ 不直接创建，发送 StateEvent::CreateAuthorityHostSession
    ▼
StateEventLoop
    │
    │ 创建 RatatuiAuthorityHostSession，注册到 AuthorityHostIoLoop
    │ 发送 LocalCatalogChangeRequest
    ▼
session sync runtime
    │
    │ 发布 TargetPublished 给 authority
    ▼
local server 更新 sidebar
```

### 12.2 authority-host session 退出

```text
shell exit
    │
    │ AuthorityHostIoLoop SIGCHLD handler
    ▼
StateEventLoop
    │
    │ 从 SharedState 移除，发送 LocalCatalogChangeRequest
    ▼
session sync runtime
    │
    │ 发布 TargetExited 给 authority
    ▼
local server 收到 TargetExited
    │
    │ signal_remote_target_exited -> StateEvent::RemoteSessionClosed
    ▼
StateEventLoop 更新 local catalog，broadcast snapshot
```

## 13. Client 通信

Client Unix socket 由 tokio runtime 处理：

```rust
tokio::spawn(async move {
    let listener = tokio::net::UnixListener::bind(socket_path)?;
    loop {
        let (stream, _) = listener.accept().await?;
        tokio::spawn(handle_client(stream, state_tx.clone()));
    }
});
```

`handle_client`：

- 连接建立 → `StateEvent::ClientConnected`
- 读取消息 → 转成对应 `StateEvent`
- 接收 StateEventLoop 发来的 snapshot → 写 socket
- 断开 → `StateEvent::ClientDisconnected`

## 14. 实施阶段

### Phase 1：StateEventLoop + AuthorityHostIoLoop

- 新增 `StateEventLoop`；
- 新增 `AuthorityHostIoLoop`，基于 `polling` crate；
- 重写 `RatatuiAuthorityHostSession`：只保留 fd + pid，不 spawn thread；
- 在 `AuthorityHostIoLoop` 中用 signal-hook 监听 SIGCHLD，检测 authority-host child exit；
- 把 `RatatuiLocalTargetFactory::create_local_target` 改为发送事件给 `StateEventLoop`；
- 验证 authority-host session exit 正确传播。

### Phase 2：Local Session 接入 StateEventLoop

- `EventProxy` 不再直接 mutate `SharedState`，改为发送 `StateEvent`；
- `handle_session_exit`  moved into `StateEventLoop`；
- 移除 `LocalSessionEvent` 和单独 worker thread；
- 验证 local session exit 行为不变。

### Phase 3：Client Handler 接入 StateEventLoop

- client socket 由 tokio 处理；
- client 命令转成 `StateEvent`；
- snapshot broadcast 通过 tokio client task 写 socket；
- 移除 per-client `std::thread::spawn`。

### Phase 4：SharedState 去锁化

- 因为所有写都在 `StateEventLoop`，逐步去掉 `SharedState` 内部 Mutex；
- client 读 snapshot 时获取只读引用或 clone。

## 15. 风险与注意事项

1. **SIGCHLD 与 `alacritty_terminal` 不冲突**  
   `alacritty_terminal::EventLoop` 会注册自己的 SIGCHLD handler；`AuthorityHostIoLoop` 也注册一个 SIGCHLD handler。Linux 下多个 signalfd/pipe 可以同时接收 SIGCHLD。收到信号后，各方只遍历自己管理的 children 调用 `try_wait()`，互不竞争。

2. **`polling` crate 与 tokio 共存**  
   gRPC / session sync 仍是 tokio async。`AuthorityHostIoLoop` 是同步 thread + `polling`。两者通过 channel 通信，边界清晰。

3. **Client socket 写阻塞**  
   snapshot broadcast 不能直接阻塞 `StateEventLoop`。通过 tokio channel 把 snapshot 发给 client task，由 client task 异步写 socket。

4. **PTY 输出批量**  
   `AuthorityHostIoLoop` 应批量读取 PTY 输出，避免每个字节都触发系统调用。

## 16. 验收标准

- `waitagent --ratatui` 启动后，authority-host session 不再为每个 session spawn reader/writer/child-waiter thread；
- 在 remote session 中输入 `exit`，远端节点 100ms 内检测到退出并发布 `TargetExited`；
- 本地 sidebar item 消失，焦点自动切换；
- 本地 local session 中输入 `exit`，server 正确 shutdown 或切换到下一个 session；
- `cargo test --release ratatui session_sync` 全部通过。
