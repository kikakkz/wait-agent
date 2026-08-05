# WaitAgent Ratatui 剪贴板粘贴设计

Version: `v1.0`  
Status: `Draft`  
Date: `2026-08-05`

## 1. 目的

为 `waitagent --ratatui` 增加剪贴板粘贴能力：用户通过 `Ctrl+V` / `Shift+P` 把剪贴板内容（文本、文件路径、图片/文件二进制）粘贴到当前 active session 的输入区。

当前行为：TUI client 只把单个按键编码成 `LogicalKey` 发给 server，没有剪贴板语义。本次设计新增 `PASTE_TEXT` / `PASTE_FILE` 两个 client→server 命令，分别处理文本/路径和文件二进制，并扩展 remote peer 之间的 gRPC 协议以支持跨节点文件粘贴。

## 2. 范围

### 2.1 包含

- 文本粘贴（`Event::Paste` 与系统剪贴板 fallback）。
- 文件路径/URI 粘贴：解析剪贴板里的文件路径，根据 session 类型决定直接发路径或先把文件内容运到 remote peer。
- 图片/文件二进制粘贴：把二进制内容缓存到 `<temp>/waitagent/`，再把缓存路径当键盘输入发给 session。
- 本地 session 与 `SessionTransport::RemotePeer` session 两种目标。
- 错误降级：读剪贴板失败、读文件失败、remote peer 拒绝/失败时的行为。

### 2.2 不包含

- 在 terminal 内直接渲染图片或文件内容（sixel / kitty image protocol 等）。
- 远程 SSH host session（`ConnectRemoteHostPaneRuntime` 创建的 shell）的文件粘贴；本次只覆盖 ratatui node 之间的 `RemotePeer` 会话。
- 剪贴板内容同步、持久化或跨 client 共享。

## 3. 术语

- **TUI client**：运行 `waitagent --ratatui` 的 UI 进程，直接与用户交互。
- **local server**：TUI client 通过 Unix socket 连接的 ratatui node server。
- **remote peer node**：`SessionTransport::RemotePeer` 会话实际运行的远端 ratatui node。

## 4. 粘贴语义

| 剪贴板内容 | 本地 session | remote peer session |
|---|---|---|
| 纯文本 | `PASTE_TEXT` 直接当键盘输入 | `PASTE_TEXT` 经 gRPC 转发给 remote peer，再当键盘输入 |
| 文件路径/URI | `PASTE_TEXT` 直接发路径字符串 | TUI client 读取本地文件 → `PASTE_FILE` 发字节 → remote peer 落地 → 输入 remote peer 本地路径 |
| 二进制文件/图片 | `PASTE_FILE` 发字节 → local server 落地 → 输入 server 本地路径 | `PASTE_FILE` 发字节 → local server 转发给 remote peer → remote peer 落地 → 输入 remote peer 本地路径 |

核心规则：

1. **文本/路径始终走 `PASTE_TEXT`**，接收方直接当键盘输入。
2. **二进制始终走 `PASTE_FILE`**，由接收方（local server 或 remote peer）写入 `<temp>/waitagent/`，再把接收方本地的绝对路径当键盘输入。
3. **remote session 的文件必须落地到 remote peer**：不能把 server/TUI 本机路径发给 remote peer。

## 5. TUI Client 剪贴板处理

### 5.1 Bracketed Paste

启用 `crossterm` 的 `bracketed-paste` 特性，初始化 terminal 时执行 `EnableBracketedPaste`，退出时执行 `DisableBracketedPaste`。

- 终端支持 bracketed paste 时，粘贴文本会触发 `Event::Paste(String)`，直接作为文本处理。
- 终端不支持或用户按 `Ctrl+V` / `Shift+P` 时，回退到系统剪贴板读取。

### 5.2 系统剪贴板读取

引入 `arboard` crate（跨平台）作为默认实现，平台工具作为 fallback：

- Linux X11/Wayland：`arboard` 优先，fallback 到 `xclip` / `wl-paste`。
- macOS：`arboard` 优先，fallback 到 `pbpaste`。
- Windows：`arboard`。

剪贴板内容类型判断：

1. 如果 clipboard 提供 URI list（`text/uri-list`），解析为文件路径列表。
2. 如果 clipboard 提供二进制内容（`image/*`、`application/octet-stream` 等），作为文件二进制处理。
3. 否则作为纯文本。

对于文件路径，TUI client 需要把 URI 解码成本地路径（例如 `file:///home/user/foo.txt` → `/home/user/foo.txt`）。

### 5.3 快捷键

- `Ctrl+V`：触发粘贴。
- `Shift+P`：同样触发粘贴（替代原本直接文本粘贴的行为，现在统一走剪贴板解析）。

只在 `Focus::Main` 时触发；sidebar、history、settings、error-log 等 overlay 打开时不触发。

## 6. Client → Local Server 协议

在现有 Unix socket 行协议上新增两条命令：

```text
PASTE_TEXT {target_id} {base64_text}
PASTE_FILE {target_id} {filename_hint} {base64_bytes}
```

### 6.1 PASTE_TEXT

- `target_id`：当前 active target 的 qualified target id。
- `base64_text`：UTF-8 文本的 base64 编码。

server 收到后直接当键盘输入发给 active session。

### 6.2 PASTE_FILE

- `target_id`：当前 active target。
- `filename_hint`：建议文件名（如剪贴板里的原始文件名，或 `paste-{timestamp}`），仅用于生成缓存路径，不包含路径分隔符。
- `base64_bytes`：文件二进制内容的 base64 编码。

server 收到后：

- 本地 session：把字节写入 `<temp>/waitagent/{filename_hint}-{random}`，然后把该绝对路径当键盘输入。
- remote session：通过 gRPC 把字节转发给 remote peer，remote peer 写入自己的 `<temp>/waitagent/` 后输入路径。

## 7. Server 内部处理

### 7.1 新增 ClientCommand 变体

```rust
pub(crate) enum ClientCommand {
    // ... existing variants
    PasteText {
        target_id: String,
        text: String,
    },
    PasteFile {
        target_id: String,
        filename_hint: String,
        bytes: Vec<u8>,
    },
}
```

### 7.2 本地 Session 处理

在 `StateEventLoop` 的 `handle_client_command` 中处理：

- `PasteText { target_id, text }`：
  - 获取 active local session；
  - 把 `text` 直接 `feed_input` 到 PTY（无需 `translate_key`）。
- `PasteFile { target_id, filename_hint, bytes }`：
  - 写入 `<temp>/waitagent/{safe_name}`；
  - 把生成的绝对路径 `feed_input` 到 PTY。

### 7.3 Remote Peer Session 处理

- `PasteText`：与现有 `Input` 类似，local server 通过 remote session 的 gRPC 输入通道把文本发给 remote peer。
- `PasteFile`：local server 把文件字节通过新增的 gRPC message 发给 remote peer；remote peer 收到后写入自己的缓存目录，然后把路径输入它自己的 PTY。

## 8. gRPC 协议扩展

在 `proto/waitagent/remote/v1/node_session.proto` 的 `NodeSessionEnvelope.body` oneof 中新增 chunked 文件粘贴消息：

```protobuf
message FilePasteChunk {
    string target_id = 1;
    string session_id = 2;
    string filename_hint = 3;
    uint64 transfer_id = 4;
    uint64 chunk_index = 5;
    uint64 total_chunks = 6;
    bytes chunk_bytes = 7;
}

message FilePasteComplete {
    string target_id = 1;
    string session_id = 2;
    string cached_path = 3;
    optional string error_message = 4;
}
```

并加入 `NodeSessionEnvelope.body`：

```protobuf
oneof body {
    // ... existing variants
    FilePasteChunk file_paste_chunk = 80;
    FilePasteComplete file_paste_complete = 81;
}
```

### 8.1 为什么需要分片

gRPC 默认单条消息大小限制为 4MB。粘贴大文件（如日志、图片、视频）时，必须把文件拆成多个 chunk 传输。建议 chunk 大小为 1MB，既能充分利用带宽，又留有足够头部余量。

### 8.2 数据流

```text
TUI client
  │ PASTE_FILE {target_id, filename_hint, bytes}
  ▼
local server
  │ 判断 target_id 为 RemotePeer
  │ 把 bytes 拆成 1MB chunks
  │ 按顺序发送 FilePasteChunk{chunk_index, total_chunks, ...}
  ▼
remote peer node
  │ 接收并缓存 chunks，校验完整性
  │ 全部收齐后写入 /tmp/waitagent/{filename_hint}-{random}
  │ 把路径输入该 target_id 对应的 PTY
  │ 回复 FilePasteComplete（cached_path / error）
  ▼
local server
  │ 可选：把结果记录到 error log；不需要回传给 TUI client
```

### 8.3 错误与取消

- 任一块传输失败或顺序错乱，receiver 丢弃该 transfer，不写入文件，不输入路径。
- Sender 在收到 `FilePasteComplete` 前不应开始新的大文件传输到同一 target，避免 receiver buffer 混乱。
- 文件名 hint 仅用于生成缓存文件名；receiver 最终使用自己的缓存目录和命名规则。

## 9. 缓存目录与文件名

### 9.1 缓存目录

```rust
fn clipboard_cache_dir() -> PathBuf {
    std::env::temp_dir().join("waitagent")
}
```

- Linux：`/tmp/waitagent/`
- macOS：`/var/folders/.../T/waitagent/`
- Windows：`%TEMP%\waitagent\`

目录不存在时自动创建。

### 9.2 文件名

```rust
format!("{}-{}", sanitize(filename_hint), random_token(8))
```

- `sanitize`：移除路径分隔符、`..`、控制字符。
- 如果 `filename_hint` 为空，使用 `paste`。
- 后缀保留原始扩展名，用于工具识别文件类型。

示例：`image-7a3f9e2d.png`。

### 9.3 缓存生命周期

本次不做自动清理。缓存文件留在 `<temp>/waitagent/` 中，由系统或用户定期清理。后续可补充 LRU 清理策略。

## 10. 错误处理

| 错误场景 | 行为 |
|---|---|
| 读系统剪贴板失败 | 在 footer 或 status message 显示简短错误，不阻塞 UI |
| 剪贴板内容无法识别 | 降级为纯文本粘贴 |
| 文件路径指向不存在的文件 | 显示错误，不发命令 |
| 写入 `/tmp/waitagent/` 失败 | 显示错误，不发路径 |
| remote peer 拒绝/失败 `FilePasteResponse.ok == false` | local server 记录 error log；TUI client 不等待响应 |
| base64 解码失败 | 按命令解析错误处理，回复 `ControlResponse::err` |

所有错误都使用 `thiserror` 类型化，并通过 `ERROR_LOG` 记录。

## 11. 并发与锁约束

依据项目 `AGENTS.md` 的并发约束：

- 剪贴板读取和文件写入是 I/O，**不能在 `StateEventLoop` 线程中同步执行**。
- TUI client 中，系统剪贴板读取应放在独立 thread / async task 中，完成后通过 channel 把结果送回主事件循环，再发命令给 server。
- Local server 中，`PasteFile` 的文件写入如果很小，可以在 `StateEventLoop` 中同步写入（因为只是本地 temp 文件），但建议同样放到独立 task 中，避免大文件阻塞事件循环。
- Remote session 的文件转发走 gRPC，本身就是异步的，不持有 `SharedState` 锁。
- `broadcast_snapshot` 仍由 `StateEventLoop` 唯一调用。

### 11.1 锁顺序

本次改动不引入新的共享锁。`StateEventLoop` 处理 `PasteText` / `PasteFile` 时，锁顺序保持现有规则：

1. `sessions.sessions`
2. `sessions.active_target`
3. `sessions.local_sessions` / `sessions.remote_sessions`

## 12. 实现阶段

### Phase 1：文本粘贴

1. `Cargo.toml` 启用 `crossterm/bracketed-paste`。
2. TUI client 初始化/清理时启用/禁用 bracketed paste。
3. 处理 `Event::Paste`，发送 `PASTE_TEXT`。
4. 绑定 `Ctrl+V` / `Shift+P`，引入 `arboard` 读取系统剪贴板文本。
5. Server 新增 `ClientCommand::PasteText`，local/remote session 都直接输入文本。
6. 验证本地和 remote session 文本粘贴。

### Phase 2：文件路径粘贴

1. TUI client 解析 `text/uri-list` 等剪贴板格式，得到本地文件路径。
2. 本地 session：直接 `PASTE_TEXT` 发路径。
3. Remote session：TUI client 读取文件内容，走 `PASTE_FILE`。
4. Server 新增 `ClientCommand::PasteFile`，本地 session 落地后输入路径。

### Phase 3：二进制文件/图片粘贴

1. TUI client 检测二进制剪贴板内容（图片、文件对象）。
2. 本地 session：走 `PASTE_FILE`，local server 落地后输入路径。
3. Remote session：走 `PASTE_FILE`，经 gRPC `FilePasteRequest` 转发到 remote peer。
4. Remote peer 实现 `FilePasteRequest` handler：落地、输入路径、回复 `FilePasteResponse`。

### Phase 4：测试与验收

1. 单元测试 `key_translation` 之外的 paste 路径解析。
2. 本地 session 文本/路径/二进制粘贴手动验收。
3. Remote peer session 文本/文件粘贴跨主机手动验收。
4. `cargo fmt --check` 与 `cargo clippy -- -D warnings` 通过。

## 13. 验收标准

- `Ctrl+V` / `Shift+P` 在主窗格触发粘贴。
- 文本粘贴到本地 bash/vim/codex 输入区内容正确，不丢失换行。
- 本地 session 文件路径粘贴：路径字符串直接出现在 PTY 输入区。
- 本地 session 图片/文件二进制粘贴：文件存到 `/tmp/waitagent/`，PTY 输入区出现该绝对路径。
- Remote peer session 文件路径/二进制粘贴：remote peer 的 `/tmp/waitagent/` 出现对应文件，remote PTY 输入区出现 remote peer 本地绝对路径。
- 剪贴板为空或不可读时，UI 给出可见提示，不 panic。
- `cargo clippy -- -D warnings` 与 `cargo test --release ratatui session_sync` 通过。
