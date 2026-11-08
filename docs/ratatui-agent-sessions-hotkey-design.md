# WaitAgent Ratatui Agent Sessions Hotkey Design

Version: `v2.0`  
Status: `Implemented`  
Date: `2026-08-08`

> v2.0 update: the implementation changed from a "unified popup" to "directly sending each agent's own session-management slash command". This design document describes the current actual behavior.

## 1. Purpose

Add a hotkey to `waitagent --ratatui` that lets the user quickly bring up the session-management interface built into the agent tool (Kimi, Codex, Claude) that owns the currently active session.

Current behavior: the user has to manually run `kimi --session`, `codex /resume`, or `claude /resume` in another terminal to manage historical sessions. This design sends the corresponding slash command directly into the current pane's input area within the TUI, and the agent itself handles listing, selection, and restore.

## 2. Scope

### 2.1 Included

- The hotkey triggers and sends an agent-specific session-management command.
- Supports Kimi Code CLI / Kimi Desktop, OpenAI Codex CLI / Codex Desktop, and Anthropic Claude Code / Claude Desktop.
- Supports both **local session** and **remote peer session** targets:
  - local session: the command is sent directly to the local server on this machine.
  - remote peer session: the command is forwarded by the local server over gRPC to the agent on the remote peer node.
- After the command is injected, an Enter keypress is appended to execute it.

### 2.2 Not included

- No unified session-list popup maintained inside waitagent.
- No reading or parsing of each agent's session data files inside waitagent.
- No management of session deletion, renaming, archiving, or similar operations.
- No agent session management for SSH remote host sessions (shells created by `ConnectRemoteHostPaneRuntime`).

## 3. Terminology

- **Agent**: an AI coding agent CLI/Desktop app such as Kimi, Codex, or Claude.
- **AgentDetector**: the existing plugin trait responsible for identifying the agent type and state of the current session from the process name and pane text.
- **Active session**: the session that has focus in the current TUI main pane.

## 4. Command mapping

| Agent | Command sent | Notes |
|---|---|---|
| Kimi | `/sessions` | Kimi Code CLI's built-in session-list slash command |
| Codex | `/resume` | Codex CLI's built-in resume slash command |
| Claude | `/resume` | Claude Code's built-in resume slash command |

The implementation lives in `send_agent_session_command` in `src/ratatui_node/client_runtime.rs`. It decides which command to send based on the `agent_command_name` of the currently active session.

## 5. Module design

### 5.1 Key code

- `src/ratatui_node/client_runtime.rs`
  - `send_agent_session_command(stream, snapshot, status_message)`: finds the corresponding `SessionView` based on `snapshot.active_target`, selects the slash command according to `agent_command_name`, injects it into the input area via `send_paste_text`, and then sends an `Enter` key event to execute it.
  - The `Ctrl+H` binding in the event loop calls the function above.

### 5.2 Relationship with the existing AgentDetector

- `AgentDetector` continues to be responsible for runtime agent identification.
- The `agent_command_name` field is determined by the output of `AgentDetector`; only sessions identified as `"kimi"`, `"codex"`, or `"claude"` trigger the corresponding command.
- Sessions identified as a plain shell or another unknown command show "no session shortcut for ...".

### 5.3 Remote peer session path

When the active session is `SessionTransport::RemotePeer`, the command is forwarded along the following path:

```
TUI client
    │
    │ PASTE_TEXT <target_id> <command>
    ▼
local ratatui node server
    │
    │ decide whether target_id is local or a remote peer
    ├─ local  → inject directly into the local session's PTY
    └─ remote → forward PASTE_TEXT to the remote peer node via NodeSessionService
                    │
                    ▼
            remote peer ratatui node
                    │
                    │ inject into the PTY of the corresponding session on the remote peer
                    ▼
            agent receives /sessions or /resume
```

Remote file paste uses a separate `PASTE_FILE` protocol; session-management commands reuse the existing text-input forwarding mechanism.

## 6. UI behavior

### 6.1 Hotkey

- Default hotkey: `Ctrl+H` (Hotkey / History).
- Only triggers when in `Focus::Main`.
- Does not trigger when an overlay is already open (sidebar focus, error log, help, etc.).

### 6.2 Action after triggering

1. After the hotkey is pressed, the TUI client obtains the `target_id` of the currently active session.
2. Looks up the corresponding `SessionView` in `snapshot.sessions`.
3. If `agent_command_name` is empty or not a known agent, the bottom status bar shows "no session shortcut for ...".
4. Otherwise, the corresponding slash command (`/sessions` or `/resume`) is sent to the target session as keyboard input, with an `Enter` event appended to execute it.
5. Subsequent list rendering, selection, and restore are all completed by the agent itself.

## 7. Error handling

| Scenario | Behavior |
|---|---|
| No active session | Status bar shows "no active session" |
| Active session not in snapshot | Status bar shows "active session not found" |
| Current session is not a known agent | Status bar shows "no session shortcut for ..." |
| Remote peer offline | Command-send failure is handled by the existing remote forwarding mechanism |
| Agent does not support the slash command | The agent reports the error itself; waitagent does not intervene |

## 8. Test strategy

### 8.1 Unit tests

- `src/ratatui_node/client_runtime.rs` retains indirect verification of `send_agent_session_command`: ensures the `Ctrl+H` event ultimately writes the correct `PASTE_TEXT` + `INPUT` command sequence.
- `src/domain/agent_detector.rs` verifies that `accepts_at_reference` correctly includes `kimi`, `claude`, and `codex`, ensuring `agent_command_name` can be set correctly.

### 8.2 Integration tests

- Press `Ctrl+H` in a real Kimi/Codex/Claude session and verify that `/sessions` or `/resume` appears in the input area and executes.
- Press `Ctrl+H` in a remote peer session and verify the command is forwarded over gRPC to the remote side and executes.

## 9. Limitations and possible follow-ups

- Each agent's slash command behavior is determined by the agent itself; waitagent is only responsible for triggering it.
- If an agent changes or removes `/sessions`/`/resume` in the future, the mapping in `send_agent_session_command` needs to be updated accordingly.
- If a unified popup is ever needed again in the future, the `AgentSessionProvider` trait, protocol messages, and popup rendering code would need to be reintroduced; those have been removed in the current code.

## 10. Historical versions

- `v1.0` (2026-08-07): the original plan implemented a unified session-list popup, including the `AgentSessionProvider` trait, `AgentSessionRegistry`, the `LIST_AGENT_SESSIONS` gRPC protocol, and remote forwarding. During implementation it was changed to the simpler slash-command approach, and the v1.0 code has been removed.
