# WaitAgent Local Acceptance Checklist

Version: `v1.0`  
Status: `Active`  
Date: `2026-04-08`

## 1. Purpose

This document defines the local acceptance gate for `T4-10`.

It is intentionally product-facing rather than implementation-facing.
The goal is to prove that one local `waitagent` workspace is usable as the real default interaction model before network-facing UX work resumes.

It complements:

- [execution-status-board.md](execution-status-board.md)
- [functional-design.md](functional-design.md)
- [interaction-flows.md](interaction-flows.md)
- [ui-design.md](ui-design.md)

## 2. Acceptance Objective

Local acceptance passes only when one `waitagent` process can reliably manage multiple shell-backed sessions in one terminal without breaking normal shell and agent interaction behavior.

In practical terms:

- The workspace shell must be usable
- Session lifecycle controls must be usable
- Shell behavior must remain natural
- Codex-like full-screen TUIs must remain usable
- Auto-switch must behave predictably enough to trust in daily use

## 3. Test Environment

Minimum local matrix:

- `bash`
- `codex`
- `claude` if available on the machine

Recommended terminal conditions:

- Standard-width terminal
- Narrow terminal resize case
- UTF-8 locale enabled

Recommended shell conditions:

- Working directory with sibling folders for completion tests
- Repository directory with enough visible output to exercise scrolling and redraw

## 4. Exit Criteria

`T4-10` may be marked `done` only when all of the following are true:

- The workspace can create, focus, list, and close multiple sessions reliably
- A shell-backed session behaves like a real reusable shell context
- Codex can start, render, navigate menus, and accept follow-up input inside WaitAgent
- Chinese input and normal UTF-8 output remain readable in validated sessions
- Peek works without stealing input ownership
- No known blocker remains in auto-switch behavior for common local workflows
- Any remaining issues are minor enough that network work would not multiply debugging cost

## 5. Scenario Checklist

### 5.1 Workspace Bootstrap

- Start `waitagent` with no explicit subcommand.
- Confirm one local workspace opens successfully.
- Confirm the initial managed shell session appears and becomes focused.
- Confirm exit from WaitAgent returns the terminal to a clean shell state.

### 5.2 Session Creation and Lifecycle

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

### 5.5 UTF-8 and Chinese Input

- Type Chinese text inside a normal shell session.
- Type Chinese text inside Codex if the workflow supports it.
- Confirm no mojibake appears during input echo or output rendering.
- Confirm mixed Chinese and ASCII text keeps visual alignment well enough for practical use.

### 5.6 Picker and Overlay Behavior

- Open the picker while another session is in the background.
- Use Up/Down to move the picker highlight.
- Confirm `Enter` activates the highlighted session.
- Confirm `Esc` closes the picker.
- Confirm picker-only keys do not leak into the shell when the picker is open.
- Confirm shell keys continue to work normally when the picker is closed.

### 5.7 Peek

- Enter Peek on a non-focused session.
- Confirm the viewed session changes while input ownership stays on the original focused session.
- Exit Peek.
- Confirm focus and input ownership return to the original session without corruption.

### 5.8 Resize

- Resize the terminal while a normal shell session is focused.
- Resize the terminal while Codex is focused.
- Confirm content remains aligned and visible after resize.
- Confirm status chrome still fits without hiding active session content unexpectedly.

## 6. Auto-Switch Acceptance Rules

This section is mandatory because auto-switch is currently the highest-risk local interaction behavior.

### 6.1 Must Hold

- Auto-switch is considered only after `Enter`.
- At most one automatic switch may happen per submitted input round.
- No automatic switch may occur while partial input is still being typed.
- Manual switching must clear the current auto-switch opportunity.

### 6.2 Same-Round Protection

- If the focused session continues the same interaction round after `Enter`, WaitAgent must stay on it.
- If the focused session produces follow-up output and then returns to its own prompt, WaitAgent must still stay on it.
- A prompt-to-prompt follow-up inside the same session must not be treated as permission to switch away.

### 6.3 Waiting-Session Switching

- If another session is already waiting and the current round truly stabilizes, one switch may occur.
- The switched-to target must be the earliest waiting session in FIFO order.
- After the switch, further auto-switching must remain blocked until the user submits new input or manually changes focus.

### 6.4 Failure Conditions

Any of the following keeps `T4-10` open:

- WaitAgent switches away from Codex or another session before the same interaction round is really done
- WaitAgent fails to switch when a clearly waiting background session should receive the one allowed switch
- WaitAgent switches more than once per submitted input round
- The lock state becomes hard to predict from user-visible behavior

## 7. Bug Capture Template

For each failed scenario, record:

- Session type: `bash`, `codex`, `claude`, or other
- Focused session before the issue
- Triggering input
- Observed result
- Expected result
- Whether the failure involves auto-switch, picker, shell fidelity, resize, or UTF-8 behavior

## 8. Current Recommendation

Until this checklist passes cleanly, local acceptance should remain the priority over mirrored network interaction work.
