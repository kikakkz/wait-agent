# Windows-as-SSH-target Support Plan (Discussion Draft)

Background: connecting to a Windows host via Ctrl+W (e.g., 192.168.1.6, already
SSH-accessible) reports `remote port probe failed`. Root cause: the entire SSH
bootstrap flow (port probe, install, credentials, start, daemon check) is made of
POSIX shell one-liners executed on the target via SSH exec; the default shell on a
Windows target is PowerShell, which cannot parse the scripts. And it is not just the
probe — the install command downloads a Linux tarball, so switching shells does not
work around it.

Note the scope boundary: this plan addresses "**installing waitagent onto a Windows
target over SSH**". "waitagent running on Windows" (phases 1-8) is already done;
"direct-connection reuse after waitagent has been manually installed on the Windows
target" (`try_reuse_existing_connection`, `remote_host_connect_runtime.rs:159`)
is cross-platform and already works, and is out of scope for this plan.

## Anatomy of the Existing Pipeline (Every Remote Command Is a POSIX One-liner)

| Step | Code location | Current state |
|---|---|---|
| Port probe | `remote_port_probe.rs:202` `remote_probe_command` | `for p in $(seq 7474 7574); do ss -ltn \| grep …` |
| Version check | `ssh_remote_host_bootstrapper.rs:631` | `command -v waitagent && waitagent --version \| grep -q <ver>` |
| ensure home | :639 | `mkdir -p $HOME/.waitagent` |
| Install preflight (proxy) | :713-734 | `curl -fsSL --connect-timeout 5 … install.sh` |
| Install/update | :577-587 | `curl -fsSL install.sh \| bash`; install.sh installs to **`/usr/local/bin`** by default (when not root, this step uses sudo; sudo is used only for this install step) |
| Credential generation | :643 | `waitagent --port P --node-key-path K --node-cert-path C __generate-node-credentials`, outputs `WAITAGENT_CREDENTIALS<pin>:<port>` |
| operator public key | `install_operator_public_key` (same file) | writes to `$HOME/.waitagent/authorized_operators/` |
| Daemon check | :692 | `ps -eo args \| grep -F -- …` |
| Start | :60-77 | `nohup waitagent --port P --node-id ID --node-key-path K --node-cert-path C __ratatui-node-server >/tmp/waitagent-P.log 2>&1 </dev/null &` + `/dev/tcp` wait for the port to be ready |
| Endpoint preflight (inbound mode) | :751 | nc / python3 / bash+timeout, pick one for the TCP probe |

All commands are **generated as strings on the control side** and delivered to the
target shell via SSH exec. The output contract has only three forms:
`port=N` / `WAITAGENT_CREDENTIALS…` / exit code. This gives a clean seam for
"generating different scripts per target platform" — the business parsing logic
(`parse_probe_output`, `parse_credentials_output`) does not need to change at all.

## Plan Overview

Introduce `RemoteShellKind { Posix, Windows }`, probe once at the start of the
connect flow, and have every subsequent command generator emit a POSIX or
PowerShell version based on the kind. The output contract, error handling,
retries, and reuse path stay unchanged.

### 1. Platform Detection (New, One SSH Exec)

```
Command: uname -s
- Exit code 0 and output is not an MSYS/MINGW/CYGWIN kernel name → Posix (covers Linux/macOS/WSL)
- Exit code 0 and output is MSYS_NT-…/MINGW*_NT-…/CYGWIN_NT-… → Windows (see below)
- Failure / not found     → Windows
```

Risks and mitigations: the target's `uname` may be the one shipped by Git for
Windows / Cygwin (native sshd + system PATH containing `C:\Program Files\Git\usr\bin`
is a common dev-machine setup). In that case the exit code is also 0 and the output
is `MSYS_NT-…` — it must be classified as Windows by **output content** (a real POSIX
kernel name never contains these tokens). A Windows machine with a genuine
Cygwin/MSYS **sshd** whose default shell is bash will still be classified as
Windows, but commands passed through bash are mangled by expansion; this is a known
limitation, documented here. The mitigation: on port-probe failure the connect
runtime re-probes with the new classification and fixes the cache (self-healing of
stale cache entries). WSL is unaffected (`uname` returns Linux, correctly). The
detection result may optionally be cached in `RemoteHostProfile` (new `remote_shell`
field) so subsequent connects skip the probe.

On Windows, execution uniformly invokes
`powershell -NoProfile -NonInteractive -Command <script>` explicitly
(or `-EncodedCommand` + UTF-16LE base64, completely avoiding quote hell),
**without relying on the target sshd's DefaultShell configuration**.

### 2. PowerShell Command Generators (Alongside the POSIX Versions, Pure Functions, Unit-testable)

| Step | PowerShell version essentials |
|---|---|
| Port probe | Iterate 7474..7574, match by Port via `[Net.NetworkInformation.IPGlobalProperties]::GetIPGlobalProperties().GetActiveTcpListeners()`; output `port=N` (contract unchanged) |
| Version check | Fixed install path `$env:USERPROFILE\.waitagent\bin\waitagent.exe` exists and `& $exe --version` matches (Windows does not rely on PATH) |
| ensure home | `New-Item -ItemType Directory -Force $env:USERPROFILE\.waitagent` |
| Install preflight | `curl.exe -fsSL --connect-timeout 5 <release zip url>` (curl.exe ships with Win10+); proxy = set `$env:ALL_PROXY=…; $env:HTTPS_PROXY=…` inside the script block before calling curl.exe |
| Install/update | `curl.exe -fsSL -o tmp.zip <waitagent-<ver>-x86_64-windows.zip>` + `tar.exe -xf tmp.zip -C <installdir>` (tar.exe is bsdtar, ships with Win10+, supports zip). Install to **`%LOCALAPPDATA%\Programs\waitagent\`** (same destination as the Windows local irm installer; write `version.txt` as the version marker). This directory is the Windows counterpart of `/usr/local/bin`: on Linux it is "system-wide, single path"; on Windows it installs per user and merges with the local install into the same path, avoiding duplicate detection; no admin needed |
| Credential generation | **Completely identical** to POSIX (runs our own exe subcommand), only paths switch to Windows form; that exe subcommand is itself cross-platform |
| operator public key | `New-Item -Force authorized_operators directory` + `Set-Content` to append the public key (need to confirm `operator_auth`'s path resolution points to `$env:USERPROFILE\.waitagent\` on Windows — it should already be home-based) |
| Daemon check | `Get-CimInstance Win32_Process -Filter "Name='waitagent.exe'"` matched by CommandLine against `--port P`/`--node-id ID`/`__ratatui-node-server` (CommandLine is readable for the same user) |
| Start | See Section 3 (the key difference) |
| Endpoint preflight | PowerShell `New-Object Net.Sockets.TcpClient` + `BeginConnect` with a 5s timeout |
| Port-ready wait | PowerShell loop trying `TcpClient` connect to 127.0.0.1:P, ~10s cap |

### 3. Startup and Process Survival (the Biggest Technical Point of This Plan)

POSIX uses `nohup … &` to detach from the SSH session. The Windows counterpart
problem: **OpenSSH for Windows's sshd runs every exec session in a kill-on-close
Job Object, so child processes are killed together when the SSH connection closes** —
`Start-Process -WindowStyle Hidden` is not enough to survive.

Candidates:

- **A. Self-daemonization (recommended)**: add a foreground/daemon fork to the
  Windows `__ratatui-node-server` entry — when stdin is detected to be an SSH
  session, use the existing phase-3 `spawn_detached`
  (`platform/process.rs`, `CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS`) **plus
  `CREATE_BREAKAWAY_FROM_JOB`** to relaunch itself, and the parent process exits
  after the port is ready. The effect aligns with `nohup &`, the start command
  barely changes, and it reuses our own process facilities. The BREAKAWAY flag
  needs to be verified/added (the current phase-3 implementation does not include
  it).
- B. schtasks one-shot task: a standard trick and it works, but it introduces a
  Task Scheduler dependency and leftover tasks, and error paths are hard to debug.
- C. Install as a Windows service: requires admin + SCM logic, the heaviest
  option; not in this round.

Option A is chosen. Logs are redirected to `%TEMP%\waitagent-<port>.log`.

### 4. Estimated File Change List

- `src/host/ssh/remote_port_probe.rs`: `remote_probe_command` dispatches by kind (PowerShell version)
- `src/host/ssh/ssh_remote_host_bootstrapper.rs`: all `*_command` generators implemented
  twice by kind; `ensure_waitagent_and_start` passes kind through; `install_operator_public_key`
  Windows version
- `src/host/ssh/remote_host_connect_runtime.rs`: detect kind at the start of connect
  (including cache read/write) and pass it through to the probe/bootstrapper
- `src/host/ssh/remote_host_history_store.rs`: profile gains an optional `remote_shell` field
  (TOML backward compatible)
- `src/platform/process.rs`: `spawn_detached` Windows branch adds `CREATE_BREAKAWAY_FROM_JOB`
- node server entry (`__ratatui-node-server`): self-daemonization fork on Windows + port-ready wait
- `operator_auth`: verify authorized_operators directory resolution on Windows
- Tests: pure-function unit tests for the command generators (isomorphic to the existing
  POSIX unit tests); real-machine E2E (192.168.1.6)

### 5. Verification Plan

1. Every step meets the same bar as the existing POSIX path: Linux `cargo test --release`,
   `clippy -D warnings`, `fmt`, and the windows-gnu check all green.
2. Command generator unit tests: the PowerShell probe/install/start scripts' output
   contract (`port=N`, `WAITAGENT_CREDENTIALS`) matches POSIX.
3. Real-machine E2E: from the control side (Windows or WSL) Ctrl+W → 192.168.1.6 (JJ
   password login) → auto download and install the Windows zip → start → create session
   → TUI shows output and accepts input. Also test the reuse path (restart the control
   side while the target's waitagent is already running; it should take the reuse dial
   and not reinstall).

### 6. Discussion Conclusions (Confirmed 2026-09-06)

1. **Process survival**: option A (self-daemonization + `CREATE_BREAKAWAY_FROM_JOB`).
2. **Target SSH requirement**: only the native OpenSSH bundled with Windows
   (Win32-OpenSSH, optional feature) is supported; MSYS/Cygwin **sshd** is a known
   unsupported case, documented here. native sshd + Git for Windows on PATH (`uname`
   answers `MSYS_NT-…` but the login shell is cmd) is classified as Windows by output
   content and works normally. On detection failure the connect runtime re-probes with
   the new classification and fixes the cache (self-healing).
3. **Install directory**: `%LOCALAPPDATA%\Programs\waitagent\` (same as the local irm
   installer, per user, no admin needed).
4. **Key login**: in verification scope together with password login (both tested in E2E).
5. **Privilege-elevation equivalent**: on Linux sudo is used only for the "install to
   /usr/local/bin" step; on Windows per-user install needs no elevation, and "run
   waitagent as administrator" is out of this round (machine-level `C:\Program Files`
   install left as a future option).
6. **No daemon supervision**: nohup only immunizes against SIGHUP (SSH disconnect); it
   does not handle restart after crash/reboot — Linux and Windows behave identically: a
   dead process is a dead process, and recovery comes from the next connect's
   `daemon_running_check` / reuse-dial failure → re-run SSH bootstrap. No
   supervisor/service is introduced in this round.

### 7. Remaining Open Questions

- Whether to cache the detection result in the profile (`remote_shell` field):
  recommended — saves one SSH exec per connect; TOML backward compatible.
- waitagent does not auto-start after a Windows machine reboot (consistent with Linux).
  Should "register into user startup items / a service" be a follow-up enhancement?
  Not in this round.

### 8. Task Breakdown

Each step must end with: Linux `cargo test --release`, `cargo clippy -- -D warnings`,
`cargo fmt --check` all green; for Windows-related code also run the mingw cross check.

- **T1 Platform detection**: `RemoteShellKind { Posix, Windows }`; one `uname -s` exec
  at the start of connect; cache the result in `RemoteHostProfile.remote_shell` (TOML
  backward compatible).
- **T2 PowerShell command generators** (the core workload): all `*_command` generators
  in `remote_port_probe.rs` and `ssh_remote_host_bootstrapper.rs` implemented twice by
  kind — port probe, version check, ensure home, install, install preflight (including
  proxy env), credential paths, operator public key, daemon check, endpoint preflight,
  port-ready wait. The output contract (`port=N` / `WAITAGENT_CREDENTIALS` / exit code)
  is unchanged; all are pure functions with unit tests isomorphic to the POSIX ones.
- **T3 Windows installer**: curl.exe downloads the release zip + tar.exe unpacks it to
  `%LOCALAPPDATA%\Programs\waitagent\`; `version.txt` version marker; a clear error
  when `curl.exe`/`tar.exe` is missing.
- **T4 Process survival**: add `CREATE_BREAKAWAY_FROM_JOB` to `spawn_detached` in
  `platform/process.rs`; `__ratatui-node-server` self-daemonizes on Windows (relaunches
  a copy of itself detached from the Job Object, parent exits after the port is ready);
  logs to `%TEMP%\waitagent-<port>.log`.
- **T5 Path verification**: `operator_auth` authorized_operators and
  `NodeCredentialPaths` resolve correctly on a Windows target (home-based paths).
- **T6 Wiring**: `remote_host_connect_runtime.rs` passes kind to the probe/bootstrapper;
  `ensure_waitagent_and_start` branches by kind; the sudo field and the MSYS scenario
  are ignored and annotated.
- **T7 README**: add a "Windows as a connection target" subsection to the
  `Remote Machines` section — steps to enable OpenSSH Server
  (`Add-WindowsCapability -Online -Name OpenSSH.Server~~~~0.0.1.0`,
  `Start-Service sshd`, firewall rules, `Set-Service -StartupType Automatic`, password/
  key authentication notes) and the limitations (requires native OpenSSH, per-user
  install without admin, no auto-start on boot).
- **T8 Verification**: all unit tests + mingw check; real-machine E2E (192.168.1.6):
  one full flow each for password login and key login (install → start → create session
  → output/input); reuse path (restarting the control side with the target already
  installed should take the reuse dial and not reinstall).

Dependencies: T1 → T2/T3/T4 can run in parallel → T5/T6 → T7 → T8.
