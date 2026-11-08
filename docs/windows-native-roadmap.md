# Bare Windows Native Application Support Roadmap

Overall goal: make WaitAgent run fully on bare Windows without relying on WSL/Cygwin/MSYS, while keeping Linux/macOS functionality from regressing.

## Current Progress

All roadmap phases (1–9) are complete. Final real-machine acceptance (2026-09-07):

- Windows host → Windows target (1.6): SSH connection works; main pane input/output normal.
- Windows host → Ubuntu target (1.9): connection succeeded; the sidebar aggregates multiple node sessions on the target machine.
- When operator authentication is rejected, connect fails immediately with cleanup guidance, no more fallback piling up nodes (v0.1.73).

## Historical Phase Records

- Phase 2 complete: local IPC switched from Unix Domain Socket to the cross-platform `LocalListener`/`LocalStream`.
  - Unix continues to use UDS.
  - Windows uses `127.0.0.1:<port>` TCP loopback + a marker file for service discovery.
  - Commands such as `node_is_running` / `list` / `stop` / `detach` work via the marker file on Windows.
  - Linux verification: `cargo test --release`, `cargo clippy -- -D warnings`, `cargo fmt --check` all green.

- Phase 3 complete: Windows process/session isolation + startup lock.
  - `platform/process.rs` Windows `spawn_detached` uses `CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS`.
  - `platform/file_lock.rs` Windows `StartupLock` uses `LockFileEx`.
  - Added `windows-sys` as a Windows-only dependency.
  - Linux verification: `cargo test --release`, `cargo clippy -- -D warnings`, `cargo fmt --check` all green.
  - Windows behavior needs verification on a real Windows environment or in CI (phase 8).

## Phase Records

### Phase 4: Windows Local PTY

- Flesh out the `platform/pty.rs` cross-platform abstraction; the Unix implementation uses `rustix_openpty`, Windows is a stub.
- Replace the `rustix_openpty` dependency in `ratatui_node/authority_host_session.rs` with `platform::pty`.
- Replace the direct Unix calls for PTY resize/set_nonblocking in `ratatui_node/authority_host_io_loop.rs`.
- Add `platform/wake_pipe.rs`, replacing the shutdown/request-wake `UnixStream::pair()` in `authority_host_io_loop` with a cross-platform implementation.
- `alacritty_terminal` promoted to a general dependency so its terminal emulator is usable on Windows too.
- `local_session.rs` platformized: Unix goes through `SHELL` and registers a process monitor; Windows goes through `powershell.exe`/`cmd.exe` and skips the monitor (to be added in phase 7).
- The Unix child spawn in `authority_host_session.rs` has been consolidated under `#[cfg(unix)]`; Windows returns a clear error, pending the phase 4b ConPTY implementation.

Acceptance criteria:
- On Windows, a local shell session can be created and terminal output is visible.
- The TUI renders local sessions correctly.
- Linux verification all green.

### Phase 4b: Windows authority-host PTY ✅ (optional, can run in parallel with phases 5/6)

- Implement ConPTY child spawn for `authority_host_session.rs` on Windows.
- Let `--connect` peers on Windows also host sessions for remote viewers.

Completed work:
- `platform/pty/windows.rs`: `ConPty` (two pairs of `CreatePipe` anonymous pipes + `CreatePseudoConsole`, with `ClosePseudoConsole` in `Drop`) and `ConPtyChild` (`WaitForSingleObject(0)` + `GetExitCodeProcess` implementing `try_wait`/`id`). `spawn_shell` spawns directly with `CreateProcessW` + `STARTUPINFOEXW` + `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE` (stable Rust's `Command` cannot express this attribute; `raw_attribute`/`spawn_with_attributes` are still nightly-only); after a successful spawn the child-end pipe handles are closed so the output pipe can reach EOF. `Cargo.toml` gains the `Win32_System_Console`/`Win32_System_Threading` features.
- `authority_host_io_loop.rs`: per-platform cfg — the Unix path (Poller + SIGCHLD) is byte-for-byte unchanged; Windows has no poller, one blocking `ReadFile` reader thread per session feeds bytes back to the IO loop via a new `PtyOutput` request variant (the thread never touches SharedState); the loop thread blocks on `recv_timeout(200ms)` and runs `check_child_exits` periodically; input is written to the ConPTY input, blocking, on the loop thread. The PTY fields of `SessionState`/`RegisterSession` are cfg'd per platform (unix=`File`, windows=`ConPty`); the child field is unified as the `SessionChild` type alias.
- `authority_host_session.rs`: real Windows `spawn` implementation (`default_shell()` reuses local_session's powershell/COMSPEC logic; the signal env enters the environment block via `apply_to_hashmap`); the `pty_master` field is replaced per platform with `conpty: Option<ConPty>`.
- `state_loop.rs` registration path cfg'd per platform (unix clones the master fd, windows `take()`s the ConPTY); the test module and `/dev/null` construction sites gated with `cfg(unix)`.

Acceptance status: all Linux verification green (`cargo test --release` 602+3+2, `cargo clippy -- -D warnings`, `cargo fmt --check`). The Windows side meets the bar by code audit; it has not been compiled/run on real Windows, deferred to phase 8.

### Phase 5: Windows agent signal ✅

- Replace the Unix datagram socket implementation in `platform/signal.rs`.
- Unix continues using UDS; Windows uses a named pipe (`\\.\pipe\waitagent-signal-<port>`).
- Add `platform::signal::default_signal_endpoint(port)` to generate the appropriate endpoint per platform.
- `runtime.rs` uses `default_signal_endpoint` to generate the signal endpoint.
- Modify `bin/waitagent-agent-signal-send.rs` to send signals over the named pipe on Windows.
- The logic in `build.rs` that skips compiling the agent signal sender C bundle is kept: on Windows the signal sender is implemented in Rust, so no C bundle is needed.

Acceptance criteria:
- agent hooks (claude/codex/kimi) can send lifecycle signals to the node server on Windows.
- Linux verification all green.

### Phase 6: UDS replacement for remote node ingress / authority ✅

Completed sub-steps:
- 6a: `platform/remote_ipc.rs` cross-platform control socket abstraction (`RemoteControlAddr`/`RemoteControlStream`/`RemoteControlListener` + tokio async versions; Unix uses UDS with filenames unchanged from history, Windows uses the `127.0.0.1` TCP loopback derived port + an ephemeral-port ready handshake).
- 6b: all owner control sockets in `remote_runtime_owner_runtime.rs` migrated to remote_ipc (including the tokio event loop, sidecar ready handshake, and startup lock).
- 6c: owner control socket migration in `remote_node_ingress_server_runtime.rs` (liveness probe, workspace registration, create-session control channel, ready handshake; in single-machine mode the acceptor supports both UDS and the new abstraction via the `OwnerControlAccept` trait).
- 6d-1: session-sync owner socket migration; the transitional listener with no callers is kept under `#[cfg(unix)]`.
- 6d-2: authority transport bridge fully cross-platformed — remote_ipc gains an authority endpoint abstraction (FNV hash naming + a Windows `.port` marker file publishing the ephemeral port + directory-scan discovery); `RemoteAuthorityTransportRuntime`, ingress `bridges: HashMap<RemoteControlAddr, …>`, and `RatatuiRemoteSession::open` are all migrated; the owner registration protocol is byte-identical on Unix.
- 6d-3a/3b: residual connect paths in `remote_session_creation_service` migrated; publication/tmux legacy/session runtime dead code gated with `#[cfg(unix)]` (verified item by item via grep); live `UnixStream::pair()` paths migrated to `socket_pair()` (Unix=UDS pair, Windows=TCP loopback pair); a repo-wide residual scan confirms all remaining UDS sites are inside `cfg(unix)` or belong to platform files / the phase 4b area.

Acceptance status: all Linux verification green (`cargo test --release` 602+5, `cargo clippy -- -D warnings`, `cargo fmt --check`). Windows-side behavior (TCP loopback replacing UDS, marker file discovery) meets the bar by code audit; it has not been compiled/run on real Windows (environment limitation: aws-lc-sys blocks cross checks), deferred to phase 8.

### Phase 7: Windows process monitoring ✅

- Replace the Linux netlink proc connector based implementation in `process_monitor/linux.rs`.
- On Windows, use WMI, ETW, or polling as alternative ways to monitor child process lifecycle.
- Keep `ProcessEvent::Fork` / `Exec` / `Exit` semantics unchanged.
- Completed work: `process_monitor/mod.rs` platform seam refactor (`PlatformPtyFd` type alias, event_loop split by linux/macos/windows, timerfd/`OwnedFd` all `#[cfg(target_os = "linux")]`); `windows.rs` implemented with Toolhelp32 snapshot diffing (new pid → `Fork`, exe path change → `Exec`, pid disappearance → `Exit`, baseline snapshot to prevent false positives, `Win32_System_Diagnostics_ToolHelp` added to dependencies); the Windows path in `local_session.rs` wired up to monitor registration. The Linux/macOS netlink/macos event_loop logic is untouched.
- Residual fixes: the unconditional `use std::os::fd::AsRawFd` in `local_session_sync_backends.rs` gated with `#[cfg(unix)]`; the `/proc` foreground process probe in `authority_host_supports_at` gated with `#[cfg(unix)]` (the Windows stub returns false, relying only on catalog records). The repo-wide audit of `std::os::unix`/`std::os::fd` residuals is complete: except for `authority_host_io_loop.rs` (which belongs to the phase 4b ConPTY scope), all are inside `cfg(unix)` or platform files.

Acceptance status: all Linux verification green (`cargo test --release` 602+3+2, `cargo clippy -- -D warnings`, `cargo fmt --check`, personally re-verified on this machine). The Windows-side Toolhelp32 implementation meets the bar by code audit; it has not been compiled/run on real Windows, deferred to phase 8.

### Phase 8: End-to-end Windows verification ✅

- Run the full flow on a real Windows machine or in CI: start the node server, create a local session, run an agent, connect to a remote host.
- Fix compatibility issues surfaced in a real Windows environment.
- Update the build docs and release process.

Acceptance criteria:
- `cargo test --release` passes on Windows (or at least with test coverage equal to Linux).
- The Windows installer/executables build and run correctly.
- No Linux/macOS regression.

Completed work:
- `.github/workflows/ci.yaml` adds `windows-check` (`cargo check --all-targets` + `cargo clippy -- -D warnings` + `cargo fmt --check`) and `windows-test` (`cargo test --release`, 533 tests), two `windows-latest` jobs.
- Rust toolchain 1.86 → 1.88 (russh's Windows dependency pageant 0.2.1 uses let-chains, stabilized only in 1.88).
- Real Windows compile/test issues fixed over 5 rounds of CI feedback: bin crate paths (`crate::` → self-contained named pipe sender); windows-sys named pipe items actually live in `Win32::System::Pipes`/`Foundation`; Unix-only imports/test code gated with `cfg(unix)`/`cfg(all(test, unix))`; the Send/Sync issue with `ConPtyChild` holding a raw `HANDLE`; alacritty_terminal 0.25.1 Windows `Pty` API differences (`child_watcher().pid()`); Windows clippy warnings brought to zero; test helpers building temp file names from thread names (contains `::`, an illegal character on Windows); `extract_agent_signal_sender` looks for a companion exe on Windows (including the test deps directory scenario); clipboard paste tests pick `PlatformContext` by host platform.
- Local cross-check toolchain: WSL + mingw (`x86_64-w64-mingw32-gcc`) makes `cargo check --target x86_64-pc-windows-gnu [--all-targets]` completable on this Linux machine in 1–2 minutes per round, so future Windows changes don't have to wait for CI.

Acceptance status: all 6 jobs green on GitHub Actions `windows-latest` (run against `47be311`, including windows-test 533 passed / 0 failed). `cargo test --release` passes on real Windows. Honest note: the interactive manual end-to-end (starting the server in a real-machine TUI, attaching a session, running an agent, connecting to a remote host) was not performed — CI tests cover the programmable parts of it (local PTY spawn, signal env, bundle extraction, paste dispatch, etc.); the remainder is manual experience verification, and any issues found will be fixed per the phase 8 process.

## Phase 9: Windows-as-SSH-target ✅

- Goal: Ctrl+W installs waitagent onto a Windows target over SSH (previously the entire bootstrap flow was POSIX shell scripts; Windows targets failed at the port-probe stage).
- Design finalized: `docs/windows-ssh-target-design.md` (pipeline anatomy, PowerShell command generator, process liveness design, discussion conclusions).
- Hard requirement: the target uses Windows' built-in native OpenSSH (Win32-OpenSSH); MSYS/Cygwin sshd is not supported. Install directory `%LOCALAPPDATA%\Programs\waitagent\` (consistent with the local irm installer, per-user, no admin rights needed); both password and key logins are within the validation scope.
- Release and real-machine acceptance (v0.1.68 → v0.1.73):
  - `ae27cba` implements the Windows SSH target (PowerShell command generator, Windows install directory, remote shell probing Git-for-Windows masquerade).
  - `8d38a2e` duplicate connect queues instead of rejecting (fixes the first-connect in-progress error).
  - `f62cd20` fixes the ingress owner control listener binding on Windows.
  - `8092bc2` fixes the reconnect-before-ingress race so it no longer piles up redundant remote nodes.
  - `fd60ca7` remote multi-node aggregation: SSH marker port enumeration + sibling dial.
  - `c7ac350` when operator authentication is rejected, connect fails immediately with cleanup guidance, forbidding fallback bootstrap node piling.
  - Real-machine verification passed (2026-09-07): Windows host → Windows target (1.6) and → Ubuntu target (1.9) both connected successfully, main pane input/output normal, all multi-node sessions appear in the sidebar; the README documents how to install OpenSSH Server on Windows targets.

## General Constraints

- Every phase change must keep Linux/macOS functionality intact.
- At the end of every phase, run:
  - `cargo fmt --check`
  - `cargo clippy -- -D warnings`
  - `cargo test --release`
- Prefer the cross-platform abstractions under `src/platform`; business code must not call OS-specific APIs such as `std::os::unix`, `libc`, or `rustix` directly.
