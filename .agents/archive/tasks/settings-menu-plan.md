# Settings Menu Plan (Ctrl-P)

## Goal
Add a TUI settings menu reachable via Ctrl-P that displays the current Listen endpoint and lets the user view/change the Public endpoint. The footer must no longer show Listen. Public may optionally be persisted to `~/.waitagent/settings.toml` with history support.

## Design Decisions (confirmed)
- Shortcut: Ctrl-P (case-insensitive, handled like Ctrl-E/O).
- Public: text input + quick clear; mutually exclusive.
- Listen: read-only display.
- Persistence: user chooses whether to save; saved value is used on next startup if no `--public` CLI flag is provided; history list of previous values is selectable.

## Files to Modify
1. `src/cli.rs` – add `load_saved_public_endpoint` helper; if no `--public`, fall back to saved value.
2. `src/infra/settings_store.rs` – NEW: persistence for public endpoint history + saved flag.
3. `src/ratatui_node/runtime.rs` – add mutable `public_endpoint_override: Mutex<Option<String>>` to `SharedState`; add helpers `advertised_public_endpoint_label()` and `set_public_endpoint()`.
4. `src/ratatui_node/snapshot.rs` – add `public_endpoint` to `FooterState`; snapshot builder reads from shared helper.
5. `src/ratatui_node/state_event.rs` – add `ClientCommand::SetPublic { endpoint: Option<String> }`.
6. `src/ratatui_node/client.rs` – parse `SET_PUBLIC <endpoint>` and `CLEAR_PUBLIC`.
7. `src/ratatui_node/state_loop.rs` – handle `SetPublic` in `handle_client_command`; broadcast snapshot.
8. `src/ratatui_node/client_runtime.rs` – add `SettingsState`, handle Ctrl-P, render settings popup, remove Listen from footer, add Ctrl-P hint.
9. `src/command/dispatch.rs` – load saved public endpoint when constructing `RemoteNetworkConfig` for node server and workspace.
10. `src/ratatui_node/mod.rs` – export `SettingsStore` if needed elsewhere.
11. `tests/cli_smoke.rs` or new unit tests in `src/infra/settings_store.rs`.

## Status
- [done] All implementation steps completed.
- [done] All automated checks pass (fmt, clippy, 458 release tests).
- [pending] Manual TUI verification (requires interactive terminal).

## Implementation Steps

### Step 1: Settings Store
- Create `src/infra/settings_store.rs` with:
  - `SettingsStore { path: PathBuf }`
  - `Settings { public_endpoint: Option<String>, public_history: Vec<String>, save_public: bool }`
  - `load()`, `save()`, `set_public(endpoint, save)`, `clear_public()`, `saved_public()` helpers.
- Default path: `waitagent_home().join("settings.toml")`.
- Add `pub mod settings_store;` in `src/infra/mod.rs`.

### Step 2: CLI fallback to saved public endpoint
- In `src/cli.rs`, add `pub fn apply_saved_public_endpoint(network: &mut RemoteNetworkConfig)` that only fills `public_endpoint` if it is `None`.
- Called from `src/command/dispatch.rs` before constructing runtimes.

### Step 3: Mutable public endpoint in SharedState
- In `SharedState`, add `pub(crate) public_endpoint_override: Mutex<Option<String>>`.
- Initialize from `network.public_endpoint` in `SharedState::new`.
- Add `pub(crate) fn advertised_public_endpoint_label(&self) -> String` that uses override if set, else `network.advertised_public_endpoint_label()`.
- Add `pub(crate) fn set_public_endpoint(&self, endpoint: Option<String>)` that updates override and the store if persistence enabled.
- Lock hierarchy: `public_endpoint_override` is standalone; no nested locks needed.

### Step 4: Snapshot/footer
- `FooterState` gains `pub public_endpoint: Option<String>`.
- `build_snapshot` sets `public_endpoint: Some(shared.advertised_public_endpoint_label())`.
- Remove `listener_endpoint` from footer rendering; keep field in `FooterState` for compatibility but set to `None`.

### Step 5: Commands
- `ClientCommand::SetPublic { endpoint: Option<String> }`.
- Parse `SET_PUBLIC <endpoint>` and `CLEAR_PUBLIC` in `src/ratatui_node/client.rs`.
- Handle in `state_loop.rs` `handle_client_command`: call `shared.set_public_endpoint(endpoint)`, broadcast snapshot, return `CommandOutcome::Message`.

### Step 6: TUI Settings Overlay
- Add `SettingsState` struct in `client_runtime.rs`:
  - `editing: String` current input
  - `selected_history: Option<usize>`
  - `save_persist: bool`
  - `focus: SettingsFocus` (Input, History, SaveCheckbox, ApplyButton, CancelButton, ClearButton)
- Ctrl-P toggles overlay.
- Render popup centered ~70%x70%, dim background.
- Sections:
  - Listen: read-only value from snapshot footer.
  - Public input: editable text (start with current public label).
  - History list: selectable previous values; selecting fills input and clears custom flag.
  - Save checkbox: when checked, Apply persists the value as the startup default. History is always updated.
  - Buttons: Apply, Clear (mutually exclusive with input), Cancel.
- Keyboard:
  - Tab/Shift-Tab cycles focus.
  - Enter activates focused button or selects history item.
  - Esc/Ctrl-P closes without applying.
  - Apply sends `SET_PUBLIC <input>` or `CLEAR_PUBLIC` if input empty.

### Step 7: Footer
- Remove Listen span from `render_footer_line`.
- Update menu text to include `Ctrl-P Settings`.

### Step 8: Dispatch integration
- In `CommandDispatcher::ratatui_node_server` and `ratatui_workspace`, load settings and apply saved public endpoint to `network` if CLI did not provide one.

## Concurrency / Lock Notes
- `SharedState.public_endpoint_override` is a single `Mutex<Option<String>>`.
- It is read in `build_snapshot` while NOT holding any other lock → safe.
- It is written only inside `StateEventLoop` in `handle_client_command` → single writer rule satisfied.
- SettingsStore I/O happens inside `StateEventLoop` writer thread; no lock held.

## Tests
1. Unit test: `settings_store` round-trips history and saved flag.
2. Unit test: `RemoteNetworkConfig::advertised_public_endpoint_label` returns override when set.
3. Unit test: `parse_command` parses `SET_PUBLIC host:port` and `CLEAR_PUBLIC`.
4. Integration: start node server, attach client, send `SET_PUBLIC`, verify snapshot footer public endpoint changes.
5. Manual: run TUI, open Ctrl-P, set public, save, restart server, verify persisted value loaded.

## Verification Commands (all passed)
```bash
cargo fmt --check              # passed
cargo clippy -- -D warnings    # passed
cargo test --release ratatui   # passed: 28 tests ok
cargo test settings_store      # passed: 4 tests ok
cargo test ratatui_node::client # passed: 5 tests ok
cargo test shared_state_public_endpoint # passed: 1 test ok
cargo test --release           # passed: 458 tests ok
```

## Manual Verification (pending)
```bash
cargo run --release -- --port 17474
# In another terminal:
cargo run --release -- __ratatui-client --port 17474
# Press Ctrl-P in TUI, set public endpoint, check persistence in ~/.waitagent/settings.toml.
```

## Rollback
- If the overlay breaks the main loop, set `SettingsState` to `None` fallback: Ctrl-P will be a no-op until fixed.
- The `listener_endpoint` field is retained in `FooterState` so older clients still deserialize.
