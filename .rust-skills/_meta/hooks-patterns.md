# Hooks Patterns Library

> Cognitive triggers for automatic reasoning processes.

## Overview

Hooks are automatic triggers that activate meta-cognition processes at key moments. They ensure reasoning happens consistently without relying on manual memory.

## Hook Categories

```
PreToolUse   → Before executing a tool
PostToolUse  → After tool execution
OnError      → When an error occurs
OnPattern    → When a pattern is detected
Periodic     → At regular intervals
```

---

## Rust-Specific Hooks

### PreToolUse Hooks

#### Before Writing Code

```yaml
trigger: [Write, Edit]
condition: target is *.rs file
actions:
  - Re-read relevant domain-* skill (if domain context exists)
  - Re-read relevant m0x skill (if fixing error)
  - Check trace.md for current understanding
purpose: Refresh constraints before implementation
```

#### Before Running Build

```yaml
trigger: [Bash with cargo build/run/test]
actions:
  - Verify current approach aligns with trace.md
  - Prepare to capture output for trace.md
purpose: Ensure intentional action
```

### PostToolUse Hooks

#### After Build/Run/Test

```yaml
trigger: [Bash with cargo]
condition: exit_code != 0
actions:
  - Parse error for E0xxx codes
  - Update trace.md with error
  - If E0xxx, initiate Layer 1 → 3 trace
  - Increment attempt counter
purpose: Error-driven learning
```

#### After Reading Docs

```yaml
trigger: [WebFetch, Read docs]
actions:
  - Extract key patterns to findings.md
  - Update trace.md with what was learned
purpose: Knowledge persistence
```

#### After Successful Build

```yaml
trigger: [Bash with cargo]
condition: exit_code == 0
actions:
  - Update trace.md with success
  - If problem was being debugged, update decision.md
purpose: Progress tracking
```

### OnError Hooks

#### Compile Error

```yaml
trigger: cargo compile error
actions:
  - Log full error to trace.md
  - Identify error code (E0xxx)
  - Load corresponding m0x skill
  - Start upward trace (Layer 1 → 2 → 3)
purpose: Systematic error handling
```

#### Runtime Panic

```yaml
trigger: panic! detected in output
actions:
  - Log panic message to trace.md
  - Identify panic location
  - Load relevant skill based on panic type
purpose: Panic debugging
```

#### Repeated Error

```yaml
trigger: same error code appears 3+ times
actions:
  - Escalate to Layer 2 analysis
  - Question current design approach
  - Consider alternative patterns
purpose: 3-Strike rule enforcement
```

### OnPattern Hooks

#### Domain Context Detected

```yaml
trigger: domain keywords in question
patterns: fintech, trading, web, embedded, cli, iot, ml
actions:
  - Load corresponding domain-* skill
  - Set Layer 3 context
  - Start downward trace if designing
purpose: Domain-aware reasoning
```

#### Error Code Detected

```yaml
trigger: E0xxx in error message
actions:
  - Map to corresponding m0x skill
  - Set Layer 1 entry point
  - Prepare for upward trace
purpose: Error-driven skill loading
```

#### Design Pattern Detected

```yaml
trigger: pattern keywords in question
patterns: repository, factory, builder, state machine
actions:
  - Load m09-domain skill
  - Load relevant m0x for implementation
  - Set Layer 2 focus
purpose: Pattern-aware reasoning
```

### Periodic Hooks

#### Context Refresh

```yaml
trigger: every 50 tool calls
actions:
  - Re-read trace.md
  - Re-read current goal
  - Verify still on track
purpose: Prevent context drift
```

#### Progress Check

```yaml
trigger: every 20 tool calls
actions:
  - Review attempts in trace.md
  - Check if stuck (same error repeated)
  - Consider escalation if needed
purpose: Progress monitoring
```

---

## Hook Implementation Patterns

### Conditional Hooks

```yaml
hook:
  trigger: [Bash]
  condition:
    command_contains: "cargo"
    exit_code: non_zero
  actions:
    - parse_error
    - update_trace
```

### Chained Hooks

```yaml
hooks:
  - name: detect_error
    trigger: cargo_error
    actions:
      - log_error
      - trigger: load_skill

  - name: load_skill
    trigger: detect_error.complete
    actions:
      - read_m0x_skill
      - trigger: start_trace
```

### Stateful Hooks

```yaml
hook:
  trigger: cargo_error
  state:
    error_count: 0
  actions:
    - increment: error_count
    - if: error_count >= 3
      then: escalate_to_layer_2
```

---

## Skill-Specific Hooks

### For m01-ownership

```yaml
hooks:
  - trigger: E0382, E0597, E0506, E0507, E0515, E0716, E0106
    actions:
      - Load m01-ownership
      - Ask: "What design led to this ownership pattern?"
      - Trace up to Layer 2/3
```

### For m07-concurrency

```yaml
hooks:
  - trigger: E0277 with Send/Sync
    actions:
      - Load m07-concurrency
      - Check for async context
      - Review thread safety requirements
```

### For unsafe-checker

```yaml
hooks:
  - trigger: unsafe keyword in code
    actions:
      - Load unsafe-checker
      - Check SAFETY comment
      - Verify invariants documented
```

---

## Anti-Pattern Prevention Hooks

### Prevent Clone Reflex

```yaml
hook:
  trigger: about to add .clone()
  condition: fixing E0382
  actions:
    - Pause
    - Ask: "Is clone the right solution?"
    - Load m01-ownership
    - Trace to Layer 2 first
purpose: Prevent surface-level fixes
```

### Prevent Unwrap Habit

```yaml
hook:
  trigger: about to add .unwrap()
  condition: not in test code
  actions:
    - Pause
    - Ask: "Should this propagate error?"
    - Load m06-error-handling
    - Consider ?, expect(), or proper handling
purpose: Prevent panic-prone code
```

### Prevent Over-Arc

```yaml
hook:
  trigger: about to wrap in Arc<Mutex<>>
  actions:
    - Pause
    - Ask: "Is shared mutable state necessary?"
    - Load m07-concurrency
    - Consider message passing alternatives
purpose: Prevent concurrency anti-patterns
```

---

## Integration with Externalization

### Hook → File Updates

| Hook | Updates |
|------|---------|
| OnError | trace.md (error log) |
| After docs read | findings.md (new knowledge) |
| After success | trace.md (progress), decision.md (if resolved) |
| Pattern detected | trace.md (context set) |

### File → Hook Triggers

| File State | Triggers |
|------------|----------|
| trace.md has 3+ same errors | escalation hook |
| findings.md has conflicting info | clarification hook |
| decision.md incomplete | reminder hook |

---

## Summary

Hooks ensure:
1. **Automatic skill loading** based on context
2. **Error tracking** and learning
3. **Context persistence** through file updates
4. **Anti-pattern prevention** through pause-and-think
5. **Progress monitoring** through periodic checks
