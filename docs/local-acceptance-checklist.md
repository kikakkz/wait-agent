# WaitAgent Local Acceptance Checklist

Version: `v1.2`  
Status: `Pending Final Manual Sign-off`  
Date: `2026-04-17`

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

## 2. Acceptance Objective

Local acceptance passes only when one `waitagent` process can reliably manage multiple shell-backed sessions in one terminal without breaking normal shell and agent interaction behavior.

In practical terms:

- the workspace shell must be usable
- session lifecycle controls must be usable
- shell behavior must remain natural
- Codex-like full-screen TUIs must remain usable
- auto-switch must behave predictably enough to trust in daily use

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
- Peek works without stealing input ownership
- no known blocker remains in auto-switch behavior for common local workflows
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

- auto-switch is considered only after `Enter`
- at most one automatic switch may happen per submitted input round
- no automatic switch may occur while partial input is still being typed
- manual switching must clear the current auto-switch opportunity

### 6.2 Same-Round Protection

- if the focused session continues the same interaction round after `Enter`, WaitAgent must stay on it
- if the focused session produces follow-up output and then returns to its own prompt, WaitAgent must still stay on it
- a prompt-to-prompt follow-up inside the same session must not be treated as permission to switch away

### 6.3 Waiting-Session Switching

- if another session is already waiting and the current round truly stabilizes, one switch may occur
- the switched-to target must be the earliest waiting session in FIFO order
- after the switch, further auto-switching must remain blocked until the user submits new input or manually changes focus

### 6.4 Failure Conditions

Any of the following keeps `T4-10` open:

- WaitAgent switches away from Codex or another session before the same interaction round is really done
- WaitAgent fails to switch when a clearly waiting background session should receive the one allowed switch
- WaitAgent switches more than once per submitted input round
- the lock state becomes hard to predict from user-visible behavior

## 7. How To Record A Failure

For each failed scenario, record:

- session type: `bash`, `codex`, `claude`, or other
- focused session before the issue
- triggering input
- observed result
- expected result
- whether the failure involves auto-switch, picker, shell fidelity, resize, or UTF-8 behavior

The detailed machine-readable verification result should then be synced into `.agents/state/last-verified.yaml` and related task state.
