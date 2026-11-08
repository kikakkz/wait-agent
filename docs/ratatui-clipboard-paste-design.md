# WaitAgent Ratatui Clipboard Paste Design

Version: `v1.0`  
Status: `Draft`  
Date: `2026-08-05`

## 1. Purpose

Add clipboard paste capability to `waitagent --ratatui`: the user presses `Ctrl+V` / `Shift+P` to paste clipboard content (text, file paths, image/file binary) into the input area of the current active session.

Current behavior: the TUI client only encodes individual keystrokes into `LogicalKey` and sends them to the server; there is no clipboard semantics. This design adds two client→server commands, `PASTE_TEXT` and `PASTE_FILE`, handling text/paths and file binary respectively, and extends the gRPC protocol between remote peers to support cross-node file paste.

## 2. Scope

### 2.1 In scope

- Text paste (`Event::Paste` and system clipboard fallback).
- File path/URI paste: parse file paths from the clipboard; depending on the session type, either send the path directly or first transfer the file content to the remote peer.
- Image/file binary paste: cache the binary content into `<temp>/waitagent/`, then send the cache path to the session as keyboard input.
- Two target types: local sessions and `SessionTransport::RemotePeer` sessions.
- Error degradation: behavior on clipboard read failure, file read failure, and remote peer rejection/failure.

### 2.2 Out of scope

- Rendering images or file content directly in the terminal (sixel / kitty image protocol, etc.).
- File paste for remote SSH host sessions (shells created by `ConnectRemoteHostPaneRuntime`); this design covers only `RemotePeer` sessions between ratatui nodes.
- Clipboard content synchronization, persistence, or cross-client sharing.

## 3. Terminology

- **TUI client**: the UI process running `waitagent --ratatui`, interacting with the user directly.
- **local server**: the ratatui node server that the TUI client connects to via Unix socket.
- **remote peer node**: the remote ratatui node where a `SessionTransport::RemotePeer` session actually runs.

## 4. Paste semantics

| Clipboard content | Local session | Remote peer session |
|---|---|---|
| Plain text | `PASTE_TEXT` directly as keyboard input | `PASTE_TEXT` forwarded to the remote peer via gRPC, then as keyboard input |
| File path/URI | `PASTE_TEXT` sends the path string directly | TUI client reads the local file → `PASTE_FILE` sends bytes → remote peer writes to disk → types the remote peer local path |
| Binary file/image | `PASTE_FILE` sends bytes → local server writes to disk → types the server local path | `PASTE_FILE` sends bytes → local server forwards to the remote peer → remote peer writes to disk → types the remote peer local path |

Core rules:

1. **Text/paths always go through `PASTE_TEXT`**; the receiver treats them directly as keyboard input.
2. **Binary always goes through `PASTE_FILE`**; the receiver (local server or remote peer) writes it into `<temp>/waitagent/` and types the receiver-local absolute path as keyboard input.
3. **Files for remote sessions must be written on the remote peer**: never send a server/TUI-local path to the remote peer.

## 5. TUI Client Clipboard Handling

### 5.1 Bracketed Paste

Enable crossterm's `bracketed-paste` feature: execute `EnableBracketedPaste` when initializing the terminal and `DisableBracketedPaste` on exit.

- When the terminal supports bracketed paste, pasted text triggers `Event::Paste(String)` and is handled directly as text.
- When the terminal does not support it, or the user presses `Ctrl+V` / `Shift+P`, fall back to reading the system clipboard.

### 5.2 System Clipboard Reading

Introduce the `arboard` crate (cross-platform) as the default implementation, with platform tools as fallback:

- Linux X11/Wayland: `arboard` first, fallback to `xclip` / `wl-paste`.
- macOS: `arboard` first, fallback to `pbpaste`.
- Windows: `arboard`.

Clipboard content type detection:

1. If the clipboard provides a URI list (`text/uri-list`), parse it as a list of file paths.
2. If the clipboard provides binary content (`image/*`, `application/octet-stream`, etc.), treat it as file binary.
3. Otherwise treat it as plain text.

For file paths, the TUI client needs to decode URIs into local paths (e.g. `file:///home/user/foo.txt` → `/home/user/foo.txt`).

### 5.3 Shortcuts

- `Ctrl+V`: triggers paste.
- `Shift+P`: also triggers paste (replaces the previous direct-text-paste behavior; everything now goes through clipboard parsing uniformly).

Triggered only in `Focus::Main`; not triggered while overlays such as sidebar, history, settings, or error-log are open.

## 6. Client → Local Server Protocol

Add two commands to the existing Unix socket line protocol:

```text
PASTE_TEXT {target_id} {base64_text}
PASTE_FILE {target_id} {filename_hint} {base64_bytes}
```

### 6.1 PASTE_TEXT

- `target_id`: the qualified target id of the current active target.
- `base64_text`: base64-encoded UTF-8 text.

The server treats it directly as keyboard input to the active session after receiving it.

### 6.2 PASTE_FILE

- `target_id`: the current active target.
- `filename_hint`: suggested filename (e.g. the original filename from the clipboard, or `paste-{timestamp}`); used only to generate the cache path and contains no path separators.
- `base64_bytes`: base64 encoding of the file binary content.

After receiving it, the server:

- Local session: writes the bytes to `<temp>/waitagent/{filename_hint}-{random}`, then types that absolute path as keyboard input.
- Remote session: forwards the bytes to the remote peer via gRPC; the remote peer writes them into its own `<temp>/waitagent/` and then types the path.

## 7. Server Internal Handling

### 7.1 New ClientCommand Variants

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

### 7.2 Local Session Handling

Handled in `StateEventLoop`'s `handle_client_command`:

- `PasteText { target_id, text }`:
  - Get the active local session;
  - `feed_input` the `text` directly to the PTY (no `translate_key` needed).
- `PasteFile { target_id, filename_hint, bytes }`:
  - Write to `<temp>/waitagent/{safe_name}`;
  - `feed_input` the generated absolute path to the PTY.

### 7.3 Remote Peer Session Handling

- `PasteText`: similar to the existing `Input`; the local server sends the text to the remote peer through the remote session's gRPC input channel.
- `PasteFile`: the local server sends the file bytes to the remote peer via a new gRPC message; after receiving them, the remote peer writes them into its own cache directory and then types the path into its own PTY.

## 8. gRPC Protocol Extension

Add chunked file paste messages to the `NodeSessionEnvelope.body` oneof in `proto/waitagent/remote/v1/node_session.proto`:

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

And add them to `NodeSessionEnvelope.body`:

```protobuf
oneof body {
    // ... existing variants
    FilePasteChunk file_paste_chunk = 80;
    FilePasteComplete file_paste_complete = 81;
}
```

### 8.1 Why Chunking Is Needed

gRPC's default single-message size limit is 4MB. When pasting large files (e.g. logs, images, videos), the file must be split into multiple chunks for transfer. The recommended chunk size is 1MB, which fully utilizes bandwidth while leaving enough header margin.

### 8.2 Data Flow

```text
TUI client
  │ PASTE_FILE {target_id, filename_hint, bytes}
  ▼
local server
  │ determine that target_id is a RemotePeer
  │ split bytes into 1MB chunks
  │ send FilePasteChunk{chunk_index, total_chunks, ...} in order
  ▼
remote peer node
  │ receive and buffer chunks, verify integrity
  │ after all chunks arrive, write to /tmp/waitagent/{filename_hint}-{random}
  │ type the path into the PTY for that target_id
  │ reply FilePasteComplete (cached_path / error)
  ▼
local server
  │ optional: record the result to the error log; no need to report back to the TUI client
```

### 8.3 Errors and Cancellation

- If any chunk transfer fails or arrives out of order, the receiver discards the transfer, writes no file, and types no path.
- The sender should not start a new large file transfer to the same target before receiving `FilePasteComplete`, to avoid receiver buffer confusion.
- The filename hint is only used to generate the cache filename; the receiver ultimately uses its own cache directory and naming rules.

## 9. Cache Directory and Filenames

### 9.1 Cache Directory

```rust
fn clipboard_cache_dir() -> PathBuf {
    std::env::temp_dir().join("waitagent")
}
```

- Linux: `/tmp/waitagent/`
- macOS: `/var/folders/.../T/waitagent/`
- Windows: `%TEMP%\waitagent\`

The directory is created automatically if it does not exist.

### 9.2 Filenames

```rust
format!("{}-{}", sanitize(filename_hint), random_token(8))
```

- `sanitize`: removes path separators, `..`, and control characters.
- If `filename_hint` is empty, use `paste`.
- The suffix keeps the original extension so tools can recognize the file type.

Example: `image-7a3f9e2d.png`.

### 9.3 Cache Lifecycle

No automatic cleanup in this iteration. Cache files stay in `<temp>/waitagent/` and are cleaned up periodically by the system or the user. An LRU cleanup strategy can be added later.

## 10. Error Handling

| Error scenario | Behavior |
|---|---|
| System clipboard read failure | Show a short error in the footer or a status message; do not block the UI |
| Unrecognizable clipboard content | Degrade to plain-text paste |
| File path points to a nonexistent file | Show an error; do not send a command |
| Writing to `/tmp/waitagent/` fails | Show an error; do not type the path |
| Remote peer rejects/fails `FilePasteResponse.ok == false` | Local server records an error log; the TUI client does not wait for a response |
| Base64 decode failure | Treat as a command parse error; reply `ControlResponse::err` |

All errors are typed with `thiserror` and recorded via `ERROR_LOG`.

## 11. Concurrency and Lock Constraints

Per the project `AGENTS.md` concurrency constraints:

- Clipboard reads and file writes are I/O and **must not execute synchronously on the `StateEventLoop` thread**.
- In the TUI client, system clipboard reads should run in a separate thread / async task; on completion, the result is sent back to the main event loop via a channel, which then sends the command to the server.
- In the local server, `PasteFile` file writes may be written synchronously in `StateEventLoop` if they are very small (since it is just a local temp file), but it is recommended to put them in a separate task as well, to avoid blocking the event loop on large files.
- Remote session file forwarding goes over gRPC, which is asynchronous by nature and does not hold the `SharedState` lock.
- `broadcast_snapshot` is still called only by `StateEventLoop`.

### 11.1 Lock Order

This change introduces no new shared locks. When `StateEventLoop` handles `PasteText` / `PasteFile`, the lock order follows the existing rules:

1. `sessions.sessions`
2. `sessions.active_target`
3. `sessions.local_sessions` / `sessions.remote_sessions`

## 12. Implementation Phases

### Phase 1: Text Paste

1. Enable `crossterm/bracketed-paste` in `Cargo.toml`.
2. TUI client enables/disables bracketed paste at init/cleanup.
3. Handle `Event::Paste` and send `PASTE_TEXT`.
4. Bind `Ctrl+V` / `Shift+P`; introduce `arboard` to read system clipboard text.
5. Server adds `ClientCommand::PasteText`; both local and remote sessions type the text directly.
6. Verify text paste in local and remote sessions.

### Phase 2: File Path Paste

1. TUI client parses clipboard formats such as `text/uri-list` to get local file paths.
2. Local session: send the path directly via `PASTE_TEXT`.
3. Remote session: TUI client reads the file content and goes through `PASTE_FILE`.
4. Server adds `ClientCommand::PasteFile`; the local session writes to disk and then types the path.

### Phase 3: Binary File/Image Paste

1. TUI client detects binary clipboard content (images, file objects).
2. Local session: go through `PASTE_FILE`; the local server writes to disk and then types the path.
3. Remote session: go through `PASTE_FILE`, forwarded to the remote peer via gRPC `FilePasteRequest`.
4. The remote peer implements the `FilePasteRequest` handler: write to disk, type the path, and reply `FilePasteResponse`.

### Phase 4: Testing and Acceptance

1. Unit tests for paste path parsing outside of `key_translation`.
2. Manual acceptance of text/path/binary paste in the local session.
3. Manual acceptance of text/file paste in the remote peer session across hosts.
4. `cargo fmt --check` and `cargo clippy -- -D warnings` pass.

## 13. Acceptance Criteria

- `Ctrl+V` / `Shift+P` triggers paste in the main pane.
- Text pasted into a local bash/vim/codex input area is correct, with no newlines lost.
- Local session file path paste: the path string appears directly in the PTY input area.
- Local session image/file binary paste: the file is stored in `/tmp/waitagent/` and the absolute path appears in the PTY input area.
- Remote peer session file path/binary paste: the corresponding file appears in the remote peer's `/tmp/waitagent/`, and the remote peer's local absolute path appears in the remote PTY input area.
- When the clipboard is empty or unreadable, the UI shows a visible prompt and does not panic.
- `cargo clippy -- -D warnings` and `cargo test --release ratatui session_sync` pass.
