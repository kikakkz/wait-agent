# WaitAgent UI Design

Version: `v1.0`  
Status: `Draft`  
Date: `2026-04-07`

## 1. Purpose

This document defines the terminal UI rules for WaitAgent.

It focuses on:

- Visual structure
- Terminal-native presentation
- Required and optional status indicators
- Focus, waiting, fullscreen, and mirrored interaction states

It complements:

- [wait-agent-prd.md](wait-agent-prd.md)
- [functional-design.md](functional-design.md)
- [interaction-flows.md](interaction-flows.md)

## 2. UI Principles

The WaitAgent UI must obey the following rules:

- The session output remains the primary surface
- The UI must feel like a terminal, not a dashboard
- One console shows one focused session at a time
- Metadata must stay lightweight and non-intrusive
- The same UI model must work in local and network mode

## 3. Terminal Layout

The base layout has three zones:

```text
┌──────────────────────────────────────────────────────────────┐
│ [devbox-1/claude-2] active | 2 waiting                      │
├──────────────────────────────────────────────────────────────┤
│                                                              │
│  Raw terminal output from the focused session                │
│                                                              │
│                                                              │
├──────────────────────────────────────────────────────────────┤
│ focus: devbox-1/claude-2 | node: devbox-1 | mode: normal     │
└──────────────────────────────────────────────────────────────┘
```

Zones:

- Top status line
- Main session viewport
- Bottom status line

The main viewport must dominate the available space.

## 4. Primary Visual Elements

### 4.1 Top Status Line

The top status line carries the minimum interaction context.

Required elements:

- Focused session identity
- Focus state
- Waiting count

Optional elements:

- Node identity
- Scheduler lock state
- Attach count
- Connectivity state

Suggested format:

```text
[devbox-1/claude-2] active | 2 waiting | lock: clear
```

### 4.2 Main Session Viewport

This is the raw screen content of the focused session.

Rules:

- Do not summarize or reinterpret output
- Do not wrap the output in cards or panes
- Do not prepend per-line prefixes
- Do not inject verbose UI between PTY frames

### 4.3 Bottom Status Line

The bottom line carries low-frequency context that helps the user stay oriented without interrupting the session viewport.

Suggested content:

- `focus`
- `node`
- `mode`
- small notices such as `remote attached` or `offline`

Suggested format:

```text
focus: devbox-1/claude-2 | node: devbox-1 | mode: normal
```

## 5. Visual States

## 5.1 Normal Focused State

The user is interacting with one focused session.

Visual requirements:

- Show focused session identity clearly
- Show waiting count if non-zero
- Keep all other UI chrome minimal

Example:

```text
[claude-2] active | 1 waiting
...session output...
focus: claude-2 | mode: normal
```

## 5.2 Waiting Present State

One or more sessions are likely waiting for user input.

Visual requirements:

- Show total waiting count
- Do not interrupt the focused viewport
- Do not forcibly reveal background session summaries

Example:

```text
[claude-2] active | 3 waiting
```

## 5.3 Explicit Selection State

The user has navigated to another session in chrome but has not yet activated it.

Visual requirements:

- Show which target is selected
- Keep the active session identity visible

Example:

```text
[claude-2] active | selected: claude-4
```

## 5.4 Fullscreen State

The main shell pane is zoomed while the workspace remains attached.

Visual requirements:

- Keep the main viewport dominant
- Preserve clear exit hints
- Preserve normal shell and TUI rendering

Example:

```text
[claude-2] active | mode: fullscreen
```

## 5.5 Remote Attach Awareness

A session may be attached by another console.

Visual requirements:

- The notice must be subtle
- The notice must not take over the viewport

Suggested indicators:

- `attached: 2`
- `remote attached`
- `remote typing`

Example:

```text
[devbox-1/claude-2] active | attached: 2
```

## 5.7 Offline Node State

If a remote node disconnects:

- Show that the node is offline
- Mark its sessions unreachable
- Do not pretend interaction is still available

Example:

```text
[devbox-2/codex-1] unreachable | node offline
```

## 6. Session Identity Rules

The UI should use the shortest form that preserves clarity.

Suggested rules:

- In local mode, use `session-id`
- In network mode, default to `node-id/session-id`
- If only one node is attached, short form may be allowed in secondary surfaces

Examples:

- `claude-2`
- `devbox-1/claude-2`
- `runner-a/codex-fix-3`

## 7. Minimal Color and Styling Rules

WaitAgent should work without color, but may optionally use restrained color.

If color is used:

- Focused session identity may be emphasized
- Waiting count may be highlighted
- Error or offline state may use warning color

Must not:

- Depend on color alone for meaning
- Use heavy gradients or decorative UI
- Create a dashboard look

## 8. Terminal Size Behavior

The UI must adapt cleanly to narrow and wide terminals.

### 8.1 Wide Terminal

Show full top and bottom status lines.

### 8.2 Narrow Terminal

Compact the status line:

```text
[claude-2] | 2 wait
```

If required:

- Drop low-priority metadata first
- Never drop focused session identity

## 9. Keyboard Interaction Surface

The UI design assumes keyboard-first control.

Core interactions:

- `Enter`
- `Ctrl + Tab`
- `Ctrl + Shift + Tab`
- `Ctrl + Number`

The UI must expose as little keyboard legend as possible by default.

Possible approach:

- No persistent shortcut help
- Short transient hint on first attach
- Explicit help command for the full key map

## 10. Optional Secondary Surfaces

The MVP should avoid adding large secondary surfaces, but two small secondary surfaces are acceptable:

### 10.1 Session Picker Overlay

Purpose:

- Fast direct focus switch

Rules:

- Full-screen modal is allowed only temporarily
- Must remain keyboard navigable
- Must list sessions in a compact terminal list, not cards

### 10.2 Node Filter Overlay

Purpose:

- Limit focus operations to one node in network mode

Rules:

- Lightweight list
- No persistent side panel

## 11. Example Screens

### 11.1 Local Focused Session

```text
[claude-2] active | 2 waiting

...raw session output...

focus: claude-2 | mode: normal
```

### 11.2 Network Mode Focused Session

```text
[devbox-1/claude-2] active | 2 waiting | attached: 2

...raw session output...

focus: devbox-1/claude-2 | node: devbox-1 | mode: normal
```

### 11.3 Fullscreen

```text
[claude-2] active | mode: fullscreen

...raw session output...

focus: claude-2 | mode: fullscreen
```

### 11.4 Unreachable Session

```text
[devbox-2/codex-1] unreachable | node offline

last known screen retained

focus: none | mode: offline
```

## 12. UI Invariants

The UI must always preserve:

- One visible focused session per console
- Raw PTY output as the dominant surface
- Explicit session identity
- Explicit state when the user is not in normal interaction mode

The UI must never:

- Show many active panes at once
- Convert agent output into summaries by default
- Blur focus ownership
- Hide network or offline state when it affects interaction
