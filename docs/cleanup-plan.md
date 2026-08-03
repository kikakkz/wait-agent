# WaitAgent Cleanup Plan (Assistant Iteration Guide)

## Invariants (never break)

1. `cargo test --release` must pass after every step.
2. `cargo clippy -- -D warnings` must pass after Phase 3 and onward.
3. Wire protocol and external env vars must stay backward-compatible unless explicitly migrated.
4. Lock hierarchy in `ratatui_node` must be preserved.
5. `SharedState` mutations must only come from `StateEventLoop`.
6. `broadcast_snapshot` must only be called by `StateEventLoop`.

## Test strategy

### Test layers

| Layer | Location | Purpose |
|-------|----------|---------|
| Unit | inline `#[cfg(test)]` in source files | business logic, protocol parsing, terminal emulation |
| Integration | `tests/*.rs` | CLI smoke, end-to-end local workspace flows |
| Concurrency model | `src/**/loom_*.rs` or `tests/loom/*.rs` | validate lock-free paths and SharedState contracts |
| E2E shell | `scripts/e2e/` | manual/automated full product scenarios |

### Required coverage before risky refactor

For every refactor, first add a **characterization test** that pins the current output. Then refactor. Then verify the test still passes.

Examples:
- Before renaming `pane_id`/`source_session_name`, add tests asserting JSON/proto serialization round-trips.
- Before splitting `SharedState`, add tests asserting session insert/remove/client attach produce the same snapshot.
- Before splitting `route_transport_envelope`, add tests for each envelope type asserting the same routing decision.

### Verification commands (run after every step)

```bash
cargo fmt --check
cargo clippy -- -D warnings
cargo test --release
```

After architecture changes additionally:

```bash
grep -R "use crate::runtime" src/application/ --include='*.rs'
grep -R "use crate::application" src/runtime/ --include='*.rs'
```

## Baseline

Record before any change:

```bash
export BASE=/tmp/waitagent-cleanup-baseline
mkdir -p $BASE
cargo test --release > $BASE/test.log 2>&1
cargo clippy -- -D warnings > $BASE/clippy.log 2>&1 || true
cargo test --release -- --list 2>/dev/null | wc -l > $BASE/test_count.txt
find src -name '*.rs' | wc -l > $BASE/file_count.txt
grep -R '#!\[allow(dead_code)\]' src --include='*.rs' -l > $BASE/dead_code_files.txt
grep -R "unsafe {" src --include='*.rs' | wc -l > $BASE/unsafe_count.txt
grep -R "// SAFETY:" src --include='*.rs' | wc -l > $BASE/safety_comment_count.txt
grep -R "unwrap()\|expect(" src --include='*.rs' | grep -v "#\[cfg(test)\]" -B5 | wc -l > $BASE/unwrap_expect_count.txt
```

Current baseline:
- tests: 406 passed, 0 failed, 1 ignored
- clippy: 62 errors
- dead_code files: 33
- unsafe blocks: ~42
- safety comments: ~some missing
- unwrap/expect in non-test code: ~70

## Target dependency graph

```text
cli -> app
app -> domain, infra, ports
ratatui_node -> domain, infra, ports, terminal
remote -> domain, infra, ports, terminal
host -> domain, infra, ports
process -> domain, infra
terminal -> std only
ports -> domain, infra
```

Forbidden after cleanup:
- `src/application/` importing `crate::runtime::*`
- `src/runtime/` importing `crate::application::*`
- `runtime/` as a catch-all module

## Phase 0: Lock baseline and target docs

### Goal
Document where we are going; freeze current state.

### Steps
1. Run baseline commands above.
2. Update `docs/architecture.md`:
   - Remove the "tmux plan is authoritative" paragraph.
   - Add ratatui runtime topology.
   - State local sessions use `alacritty_terminal`, remote sessions use `TerminalEngine` via `RemoteObserverRuntime`.
3. Update `docs/module-design.md`:
   - Add note: `renderer`/`console` currently live in `runtime::ratatui_node`, target is top-level.
   - Add planned modules: `ratatui_node`, `remote`, `host`, `process`.

### Tests to add
None; docs only.

### Verification
```bash
grep -q "ratatui" docs/architecture.md && echo OK
grep -q "tmux plan is authoritative" docs/architecture.md && echo FAIL || echo OK
[ -f $BASE/test.log ] && echo baseline recorded
```

## Phase 1: Mechanical tmux cleanup

### Goal
Remove tmux-era terminology from active code without changing behavior.

### Characterization tests (add first)
1. In `src/runtime/ratatui_node/agent_signal_env.rs`, add test `agent_signal_env_serializes_pane_id`:
   ```rust
   #[test]
   fn agent_signal_env_serializes_pane_id() {
       let env = AgentSignalEnv {
           socket_path: "/tmp/s".into(),
           socket_name: "ratatui-1".into(),
           target_session_name: "sess".into(),
           pane_id: "sess".into(),
           token: "tok".into(),
       };
       let mut map = HashMap::new();
       env.apply_to_hashmap(&mut map).unwrap();
       assert_eq!(map.get("WAITAGENT_PANE_ID"), Some(&"sess".to_string()));
   }
   ```
2. In `src/bin/waitagent-agent-signal-send.rs`, add test `signal_json_includes_pane_field`:
   ```rust
   #[test]
   fn signal_json_includes_pane_field() {
       let json = build_signal_json("sess", "sig");
       assert!(json.contains("\"pane\":\"sess\""));
   }
   ```
3. In `src/infra/remote_protocol.rs`, add round-trip tests for every struct containing `source_session_name`.

### Steps
1. Rename `pane_id` -> `session_id` in:
   - `src/runtime/ratatui_node/agent_signal_env.rs`
   - `src/bin/waitagent-agent-signal-send.rs`
   - `src/runtime/ratatui_node/agent_signal_server.rs`
2. Keep `WAITAGENT_PANE_ID` env var as deprecated alias; add `WAITAGENT_SESSION_ID`.
3. Rename `source_session_name` -> `authority_host_session_name` in:
   - `src/infra/remote_protocol.rs`
   - `src/infra/remote_transport_codec.rs`
   - `src/runtime/remote_node/*`
   - `src/runtime/remote_publication/*`
4. Delete dead files listed in baseline.
5. Update tmux comments in active runtime code.
6. `cargo clean`.

### Safety rules
- Any env var or JSON field visible to external processes keeps a deprecated alias for one release.
- Do not change gRPC/proto field wire names.

### Tests to add/update
- Update characterization tests to assert new names while keeping backward compatibility tests.
- Add `agent_signal_env_serializes_session_id` asserting `WAITAGENT_SESSION_ID`.
- Add `signal_json_includes_session_field`.

### Verification
```bash
cargo test --release
cargo check
grep -R "tmux\|TMUX" src/ --include='*.rs' | grep -v "Legacy tmux-era" | grep -v "historical"
# expected: only comments
```

## Phase 2: Reconnect or delete orphan tests

### Goal
No test file exists that is not compiled.

### Characterization tests
Before touching orphan files, run them manually once to see what they currently assume:

```bash
# temporary: add mod declaration to parent, run, record failures
```

### Steps
1. For each orphan file, decide restore or delete:
   - `remote_authority_target_host_runtime_test.rs`: choose between this and inline test module.
   - others: restore by adding `mod ..._test;` in parent, or delete if tmux-specific.
2. Wire restored tests into parent modules.
3. Fix compilation errors.
4. Add tests for core ratatui concurrency.

### Tests to add

#### A. `src/runtime/ratatui_node/state_loop.rs`
Add module `#[cfg(test)] mod state_loop_tests` with:

```rust
#[test]
fn state_loop_broadcasts_snapshot_after_local_session_output() {
    // Setup SharedState + StateEventLoop in a thread.
    // Send LocalSessionOutput event.
    // Assert broadcast_snapshot is called exactly once and contains the session.
}

#[test]
fn state_loop_rejects_direct_shared_state_mutation_outside_loop() {
    // Compile-time check: SharedState fields are private except to StateEventLoop.
}

#[test]
fn state_loop_preserves_lock_order_clients_then_sessions() {
    // Spawn threads that simulate client attach and session update.
    // Run under loom or deterministic stress.
}
```

#### B. `src/runtime/ratatui_node/snapshot.rs`
```rust
#[test]
fn snapshot_serializes_and_deserializes() {
    let snap = build_sample_snapshot();
    let bytes = serialize(&snap);
    let decoded = deserialize(&bytes);
    assert_eq!(snap, decoded);
}
```

#### C. `src/runtime/ratatui_node/local_session.rs`
```rust
#[test]
fn local_session_spawns_and_emits_state_events() {
    // Use SHELL=/bin/sh, spawn, wait for initial output, verify state events contain session id.
}
```

#### D. `src/runtime/ratatui_node/remote_session.rs`
```rust
#[test]
fn remote_session_opens_mirror_and_receives_bootstrap() {
    // Mock UnixListener as authority transport.
    // Call RatatuiRemoteSession::open with a test ManagedSessionRecord.
    // Feed OpenTargetOk + MirrorBootstrapChunk.
    // Assert observer snapshot contains expected screen content.
}
```

#### E. `tests/cli_smoke.rs`
```rust
#[test]
fn cli_help_returns_zero() {
    let output = Command::new(env!("CARGO_BIN_EXE_waitagent"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(output.status.success());
}
```

#### F. Domain detectors
In each of:
- `src/domain/agent_detector_claude.rs`
- `src/domain/agent_detector_codex.rs`
- `src/domain/agent_detector_shell.rs`

Add:
```rust
#[test]
fn detects_input_state_from_prompt_line() { ... }
#[test]
fn detects_running_state_from_output() { ... }
#[test]
fn ignores_irrelevant_lines() { ... }
```

### Verification
```bash
cargo test --release
cargo test --release session_sync  # should match tests now
find src -name '*_test.rs' | while read f; do
  parent=$(dirname "$f")/$(basename "$f" _test.rs).rs
  [ -f "$parent" ] && grep -q "mod .*_test" "$parent" && echo OK "$f" || echo ORPHAN "$f"
done
```

## Phase 3: Clippy and dead_code

### Goal
Clean quality gate.

### Characterization tests
Before removing `#![allow(dead_code)]`, run:

```bash
cargo build --release 2>&1 | grep "dead_code" > $BASE/dead_code_warnings.txt
```

This tells us which items will surface. For each item, either:
- delete it,
- add `#[allow(dead_code)]` with a TODO, or
- add a test that uses it.

### Steps
1. Fix clippy errors by category.
2. Remove `#![allow(dead_code)]` from all files.
3. Replace with item-level `#[allow(dead_code)]` + TODO for truly transitional items.
4. Remove `#![allow(unused_imports)]` from `src/terminal/mod.rs`.

### Tests to add
For every item that surfaces as dead_code:
- If it is used only in tests, move it under `#[cfg(test)]`.
- If it is transitional, add a TODO issue link and keep `#[allow(dead_code)]`.
- Otherwise delete it.

### Verification
```bash
cargo clippy -- -D warnings
cargo test --release
grep -R '#!\[allow(dead_code)\]' src --include='*.rs' | wc -l  # expect 0
```

## Phase 4: Break runtime/application cycle

### Goal
`application` does not depend on `runtime` and vice versa.

### Characterization tests
Before moving traits, add tests that exercise the current concrete service behavior:

1. In `src/application/remote_session_creation_service.rs`, add test `creates_session_via_runtime`.
2. In `src/application/target_registry_service.rs`, add test `registers_target`.
3. In `src/runtime/ratatui_node/state_loop.rs`, add test `state_loop_invokes_session_creation_service`.

These tests must continue to pass after the trait extraction; only import paths change.

### Steps
1. Create `src/ports/` with traits.
2. Move trait definitions from `application/`.
3. Implement traits in `runtime/`.
4. Update `application/` to use `Arc<dyn Port>`.
5. Update `runtime::ratatui_node` to import from `ports/`.

### Tests to add
1. `src/ports/session_creation.rs`:
   ```rust
   #[test]
   fn session_creation_port_object_safe() {
       let _: Box<dyn SessionCreationPort> = Box::new(MockSessionCreation);
   }
   ```
2. Mock implementations in `src/ports/test_doubles.rs`.
3. Update existing tests to use mocks instead of concrete runtime.

### Verification
```bash
grep -R "use crate::runtime" src/application/ --include='*.rs' | wc -l  # expect 0
grep -R "use crate::application" src/runtime/ --include='*.rs' | wc -l  # expect 0
cargo test --release
cargo clippy -- -D warnings
```

## Phase 5: Split runtime/ module

### Goal
`runtime/` disappears or becomes a thin shim.

### Characterization tests
Before moving modules, add integration-level tests that exercise the public API surface:

1. `tests/module_smoke.rs`:
   ```rust
   #[test]
   fn ratatui_node_runtime_is_reachable() {
       // Use the public API that will be re-exported.
   }
   ```
2. For each moved module, add a test in its new location that imports it via the new path.

### Steps
1. Move `src/runtime/ratatui_node/` -> `src/ratatui_node/`.
2. Add temporary re-export in `src/runtime/mod.rs`.
3. Move network code to `src/remote/`.
4. Move host code to `src/host/`.
5. Move process code to `src/process/`.
6. Update `src/main.rs`.

### Tests to add
1. `tests/topology.rs`:
   ```rust
   #[test]
   fn ratatui_node_module_exists() {
       use waitagent::ratatui_node::runtime::SharedState;
   }
   #[test]
   fn remote_module_exists() {
       use waitagent::remote::transport::LocalNodeMailbox;
   }
   ```

### Verification
```bash
cargo test --release
cargo clippy -- -D warnings
find src/runtime -name '*.rs' | wc -l  # expect 0 or 1
```

## Phase 6: Reduce god objects and oversized functions

### Goal
No struct/function carries unrelated responsibilities.

### Characterization tests
Before splitting `SharedState`:

1. `src/runtime/ratatui_node/runtime.rs`:
   ```rust
   #[test]
   fn shared_state_snapshot_matches_manual_fields() {
       let state = build_sample_shared_state();
       let snap = build_snapshot(&state);
       // Pin exact snapshot content.
   }
   ```
2. Before splitting `route_transport_envelope`, add a test for each envelope type.
3. Before splitting `run_state_event_loop`, add a test for each event variant.

### Steps
1. Refactor `SharedState` into `SessionRegistry`, `ClientRegistry`, `AgentSignalState`.
2. Split `connect_remote_host_pane_runtime.rs`.
3. Split `remote_node_ingress_server_runtime.rs`.
4. Split `remote_runtime_owner_runtime.rs`.
5. Reduce `route_transport_envelope` and `run_state_event_loop`.

### Tests to add
1. `src/ratatui_node/session_registry.rs`:
   ```rust
   #[test]
   fn insert_and_get_session() { ... }
   #[test]
   fn remove_session_notifies_clients() { ... }
   ```
2. `src/ratatui_node/client_registry.rs`:
   ```rust
   #[test]
   fn add_client_broadcasts_snapshot() { ... }
   #[test]
   fn remove_client_updates_count() { ... }
   ```
3. `src/remote/node/ingress_server.rs`:
   ```rust
   #[test]
   fn route_transport_envelope_routes_target_output() { ... }
   #[test]
   fn route_transport_envelope_routes_bootstrap_chunk() { ... }
   ```

### Verification
```bash
cargo test --release
cargo clippy -- -D warnings
# manual: no function > 100 lines without justification comment
```

## Phase 7: Deduplicate code

### Goal
Eliminate copy-paste.

### Characterization tests
Before merging hooks services, add tests to each existing service:

```rust
#[test]
fn claude_hooks_reconcile_preserves_waitagent_hooks() { ... }
#[test]
fn codex_hooks_reconcile_preserves_waitagent_hooks() { ... }
#[test]
fn kimi_hooks_reconcile_preserves_waitagent_hooks() { ... }
```

These must pass before and after the generic refactor.

### Steps
1. Merge hooks config services into generic `AgentHooksConfigService`.
2. Extract `src/domain/agent_detector_common.rs`.
3. Extract `src/process/startup_lock.rs`.
4. Extract `src/process/session_leader.rs`.

### Tests to add
1. `src/application/agent_hooks_config_service.rs`:
   ```rust
   #[test]
   fn generic_service_reconciles_claude_settings() { ... }
   #[test]
   fn generic_service_reconciles_codex_hooks() { ... }
   #[test]
   fn generic_service_reconciles_kimi_hooks() { ... }
   ```
2. `src/process/startup_lock.rs`:
   ```rust
   #[test]
   fn startup_lock_acquire_and_release() { ... }
   ```
3. `src/domain/agent_detector_common.rs`:
   ```rust
   #[test]
   fn common_scanner_detects_prompt_characters() { ... }
   ```

### Verification
```bash
cargo test --release
cargo clippy -- -D warnings
# duplicate line count should decrease
```

## Phase 8: Error handling and unsafe hygiene

### Goal
No silent drops; all unsafe documented.

### Characterization tests
Before changing error handling, add tests that exercise current error paths:

1. `src/runtime/ratatui_node/authority_host_io_loop.rs`:
   ```rust
   #[test]
   fn output_tx_none_does_not_panic() { ... }
   ```
2. `src/runtime/remote_node/remote_runtime_owner_runtime.rs`:
   ```rust
   #[test]
   fn poisoned_mutex_does_not_panic() { ... }
   ```

### Steps
1. Audit `unwrap/expect` (~70 occurrences).
2. Audit `let _ = ...` patterns.
3. Add `// SAFETY:` to every `unsafe` block.
4. Evaluate replacing hand-rolled termios.

### Tests to add
1. Error propagation tests for each replaced `expect`.
2. `terminal/platform.rs`:
   ```rust
   #[test]
   fn current_size_returns_error_when_not_tty() { ... }
   ```

### Verification
```bash
cargo clippy -- -D warnings
cargo test --release
unsafe_count=$(grep -R "unsafe {" src/ --include='*.rs' | wc -l)
safety_count=$(grep -R "// SAFETY:" src/ --include='*.rs' | wc -l)
[ "$unsafe_count" -eq "$safety_count" ] && echo OK || echo "missing safety comments"
```

## Phase 9: Docs and external artifacts

### Goal
External-facing docs match implementation.

### Steps
1. Update `README.md`.
2. Update packaging descriptions.
3. Archive historical tmux docs.
4. Clean git state.
5. Mark E2E scripts.
6. Create ratatui smoke E2E.

### Tests to add
1. `tests/readme_smoke.rs`:
   ```rust
   #[test]
   fn readme_does_not_claim_tmux_native() {
       let readme = include_str!("../README.md");
       assert!(!readme.contains("tmux-based"));
   }
   ```
2. `scripts/e2e/e2e-ratatui-smoke.sh`:
   - Start waitagent in ratatui node mode.
   - Create a local session.
   - Verify session appears in snapshot.
   - Kill session.

### Verification
```bash
bash scripts/e2e/e2e-ratatui-smoke.sh
grep -R "tmux-native\|tmux-first" scripts/ docs/ README.md | grep -v archive
# expect empty
grep -R "tmux" .git/config .gitmodules || echo OK
```

## Phase 10: Concurrency hardening

### Goal
Validate `AGENTS.md` concurrency contracts.

### Characterization tests
Before adding loom, document current lock-free paths:

1. `src/ratatui_node/runtime.rs`: list all channels.
2. `src/ratatui_node/state_loop.rs`: list all lock acquisition orders.

### Steps
1. Add `loom` as dev dependency.
2. Model test lock-free channels.
3. Add stress tests for `SharedState`.
4. Verify/document lock hierarchy.

### Tests to add

#### `src/ratatui_node/loom_state_loop.rs`
```rust
#[cfg(test)]
mod loom_tests {
    use loom::thread;

    #[test]
    fn concurrent_session_insert_and_client_attach() {
        // Two threads: one inserts session, one attaches client.
        // Verify no deadlock and snapshot is consistent.
    }

    #[test]
    fn broadcast_snapshot_single_caller() {
        // Verify via instrumentation that only StateEventLoop calls broadcast_snapshot.
    }
}
```

#### `src/ratatui_node/loom_channels.rs`
```rust
#[test]
fn state_event_channel_delivers_in_order() {
    // loom model for mpsc::Sender<StateEvent> + Receiver.
}
```

### Verification
```bash
cargo test --release
cargo test --release --features loom  # or equivalent loom command
grep -R "broadcast_snapshot" src/ratatui_node --include='*.rs' -l | while read f; do
  [ "$f" = "src/ratatui_node/state_loop.rs" ] || echo "unexpected caller: $f"
done
```

## Per-step safety checklist

Run before declaring any phase done:

```bash
cargo fmt --check
cargo clippy -- -D warnings
cargo test --release
```

For architecture phases (4, 5, 6) also run:

```bash
grep -R "use crate::runtime" src/application/ --include='*.rs'
grep -R "use crate::application" src/runtime/ --include='*.rs'
```

## Definition of done

- [ ] `cargo test --release` passes
- [ ] `cargo clippy -- -D warnings` passes
- [ ] No orphan test files
- [ ] No `tmux`/`TMUX` in active `src/` code
- [ ] `src/application/` does not import `crate::runtime`
- [ ] `src/runtime/` gone or shim-only
- [ ] `SharedState` split into sub-structs
- [ ] Every `unsafe` block has `// SAFETY:`
- [ ] `README.md` describes ratatui workspace
- [ ] Ratatui smoke E2E passes
- [ ] Loom tests pass
