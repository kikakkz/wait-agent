# WaitAgent Local Acceptance Checklist

Version: `v1.4`  
Status: `Signed Off`  
Date: `2026-04-30`

## 1. Purpose

This document is the human-run acceptance checklist for `T4-10`.

It is intentionally product-facing.
Detailed machine verification state now lives in `.agents/state/last-verified.yaml`.

Use this file for:

- what a human should validate
- what counts as acceptance
- what kinds of failures keep the gate open

Daemon detach and reattach persistence, multi-attach behavior, and host-wide daemon listing are tracked separately in the lifecycle task queue and are not part of this checklist's pass criteria.

Do not use this file as a machine verification ledger.

The checklist remains a regression reference after sign-off, but it is no longer the active project gate.

## 2. Acceptance Objective

Local acceptance passes only when one `waitagent` process can reliably manage multiple shell-backed sessions in one terminal without breaking normal shell and agent interaction behavior.

In practical terms:

- the workspace shell must be usable
- session lifecycle controls must be usable
- shell behavior must remain natural
- Codex-like full-screen TUIs must remain usable

## 3. Recommended Environment

Recommended validation matrix:

- `bash`
- `codex`
- `claude` if available on the machine

Recommended terminal conditions:

- standard-width terminal
- narrow terminal resize case
- UTF-8 locale enabled

Recommended shell conditions:

- a working directory with sibling folders for completion tests
- a repository directory with enough visible output to exercise scrolling and redraw

## 4. Exit Criteria

`T4-10` may be marked `done` only when all of the following are true:

- the workspace can create, focus, list, and close multiple sessions reliably
- a shell-backed session behaves like a real reusable shell context
- Codex can start, render, navigate menus, and accept follow-up input inside WaitAgent
- Chinese input and normal UTF-8 output remain readable in validated sessions
- any remaining issues are minor enough that network work would not multiply debugging cost

## 5. Scenario Checklist

### 5.1 Workspace Bootstrap

- Start `waitagent` with no explicit subcommand.
- Confirm one local workspace opens successfully.
- Confirm the initial managed shell session appears and becomes focused.
- Confirm exit from WaitAgent returns the terminal to a clean shell state.

### 5.2 Session Creation And Lifecycle

- Create a second session from inside the workspace.
- Create a third session from inside the workspace.
- Open the session picker and confirm all live sessions are listed.
- Focus a session by picker selection.
- Focus a session by next/previous navigation.
- Close the focused session.
- Confirm focus falls back to another valid live session.
- Confirm exited sessions disappear from active scheduling targets.

### 5.3 Shell Fidelity

- Run normal shell commands such as `pwd`, `ls`, and `echo`.
- Change directories inside one session with `cd`.
- Confirm the session-specific working directory is preserved when returning to that session.
- Use `Tab` completion for paths.
- Use `Backspace`, arrow keys, and spaces during normal shell editing.
- Confirm shell editing behavior remains natural and no synthetic spaces or dropped characters appear.

### 5.4 Codex TUI Fidelity

- Start `codex` inside a managed session.
- Confirm the startup menu renders.
- Confirm Up/Down navigation works inside Codex menus.
- Confirm `Enter` works inside Codex menus.
- Confirm the follow-up Codex interaction box remains usable after menu submission.
- Confirm WaitAgent does not steal focus away during the same Codex interaction round.
- Confirm cursor visibility looks natural while navigating Codex.
- Confirm the Codex viewport is not vertically clipped by WaitAgent chrome.

### 5.5 UTF-8 And Chinese Input

- Type Chinese text inside a normal shell session.
- Type Chinese text inside Codex if the workflow supports it.
- Confirm no mojibake appears during input echo or output rendering.
- Confirm mixed Chinese and ASCII text keeps visual alignment well enough for practical use.

### 5.6 Picker And Overlay Behavior

- Open the picker while another session is in the background.
- Use Up/Down to move the picker highlight.
- Confirm `Enter` activates the highlighted session.
- Confirm `Esc` closes the picker.
- Confirm picker-only keys do not leak into the shell when the picker is open.
- Confirm shell keys continue to work normally when the picker is closed.

### 5.7 Fullscreen

- Enter fullscreen from the main shell pane.
- Exit fullscreen back to the fixed workspace chrome.
- Confirm the footer or status hints remain readable in both states.
- Confirm fullscreen preserves usable shell and Codex interaction.

### 5.8 Resize

- Resize the terminal while a normal shell session is focused.
- Resize the terminal while Codex is focused.
- Confirm content remains aligned and visible after resize.
- Confirm status chrome still fits without hiding active session content unexpectedly.

## 6. How To Record A Failure

For each failed scenario, record:

- session type: `bash`, `codex`, `claude`, or other
- focused session before the issue
- triggering input
- observed result
- expected result
- whether the failure involves picker, fullscreen, shell fidelity, resize, or UTF-8 behavior

The detailed machine-readable verification result should then be synced into `.agents/state/last-verified.yaml` and related task state.

## 7. Phase 2 Remote Validation Extension

This appendix is the human-run validation checklist for `task.t5-08c`.

It does not reopen the local `T4-10` gate.
It exists so the accepted phase-2 cross-host path can be validated against the
same visible-behavior standard before the network MVP is marked complete.

### 7.1 Recommended Environment

- one server host running WaitAgent with an explicit listener port such as `waitagent --port 7474`
- one separate authority host running WaitAgent with outbound remote targeting such as `waitagent --port 7474 --connect <server-host>:7474`
- one local workspace console and one dedicated server-console surface available as independent product surfaces
- a shell-backed target first, then `codex` or another TUI once the shell path passes

### 7.2 Workspace Remote Target Open

- Publish one remote target from the authority host and confirm it appears in the shared catalog on the local workspace host without requiring any pre-existing local publication binding for that target.
- Open that remote target from the normal workspace sidebar or picker path.
- Confirm the main slot enters the remote surface instead of falling back to a local attach path.
- Confirm the placeholder state changes from waiting to connected once the authority session is live.
- Run `echo remote-ok` on the authority-side PTY and confirm the bytes become visible in the workspace main slot.

### 7.3 Workspace Remote Input And Resize

- Type normal shell input into the opened remote target from the workspace surface.
- Confirm the authority-side PTY receives that input exactly once.
- Resize the local terminal while the remote target is focused.
- Confirm the remote viewport redraws cleanly and the authority-side PTY receives the explicit resize.
- Confirm later remote output still appends in order after the resize.

### 7.4 Server Console Remote Interaction

- Start `waitagent --port 7474` on the server host.
- Open the same remote target from the server-console picker.
- Confirm the remote target renders in the server-console interaction surface rather than only updating hidden state.
- Type input from the server-console surface and confirm the authority-side PTY receives it.
- Confirm `Ctrl-]` returns to the picker without breaking the remote target or corrupting later reopen behavior.

### 7.5 Disconnect And Reconnect

- While the remote target is open, stop or sever the authority-side node session.
- Confirm the visible surface reports the authority disconnect instead of silently freezing.
- Confirm the published target remains present but becomes unavailable or offline in catalog-driven surfaces.
- Restore the authority-side node session.
- Confirm the target becomes reachable again without requiring local catalog surgery.
- Confirm new authority output becomes visible again after reconnect.

### 7.6 Detach And Reattach Continuity

- With one live remote node connected and at least one remote session visible in the sidebar, detach the current local WaitAgent client.
- Reattach to the same local backend.
- Confirm the same live remote session rows reappear without requiring a reconnect command or any cache-file recovery.
- Stop the owning local backend completely.
- Start a fresh local WaitAgent backend without reconnecting any remote node.
- Confirm remote session rows are empty on cold start instead of replaying stale state from an earlier run.

### 7.7 TUI Follow-Up

- Start `codex` or another full-screen TUI inside the remote authority PTY.
- Confirm the remote workspace surface renders the TUI without obvious corruption.
- Confirm basic navigation plus submit input still works through the remote path.
- Confirm server-console reopen of the same remote target still shows continued output instead of a dead surface.
