# Project Constraints

## Analysis and Reasoning Style

- All analysis must be grounded in observable facts: logs, stack traces, code paths, compilation errors, or reproduced behavior.
- Do not use speculative qualifiers such as “maybe”, “possibly”, “likely”, “probably”, or “might” when explaining root causes or behavior.
- State what the code does, what the failure is, and what the evidence is. If the root cause is not yet proven, say so explicitly and list the next concrete verification step instead of guessing.

## Rust Concurrency and Shared State

When modifying code that uses threads, mutexes, channels, or event loops in this project:

- **`SharedState` has a single writer**. All mutations to `SharedState` must go through `StateEventLoop`. Do not mutate `SharedState` directly from `EventProxy`, client handlers, control commands, or any other thread.
- **Lock order is part of the design**. Before adding or reordering locks, document the lock hierarchy in a code comment and verify that every code path acquires locks in the same order.
- **Never hold a lock while calling a callback, broadcasting, or doing I/O**. In particular:
  - `clients` lock must not be held while calling `build_snapshot` or `broadcast_snapshot`.
  - `sessions` lock must not be held while calling `broadcast_snapshot`.
  - `Term` lock must not be held while calling `broadcast_snapshot`.
- **Prefer message passing over shared mutable state**. Use channels to communicate between loops; keep mutexes for data wholly internal to one thread/loop.
- **Verify concurrency changes**. After touching concurrency code, run:
  - `cargo clippy -- -D warnings`
  - `cargo test --release ratatui session_sync`
  - If the change affects lock-free paths, add or run a `loom` model test.

Reference skill: [actionbook/rust-skills m07-concurrency](.rust-skills/skills/m07-concurrency/SKILL.md).
