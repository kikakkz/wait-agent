# AGENTS.md

wait-agent is a self-hosted agent workspace runtime with a TUI console and
node-to-node remote sessions. This file is the operating contract for every
contributor, human or AI. It is optimized for constraints and commands;
bulky knowledge lives in `docs/` and is referenced by pointer.

## Hard constraints (non-negotiable)

1. **Issue-driven development.** No issue, no code — and "no issue yet"
   never means skip: before any development, find or file the issue first
   (bots and agents too; CI cannot see the tracker, so this is on the
   contributor). Every PR references an issue (`Closes #N`). Branches are
   named `<type>/<issue>-<slug>` where `<type>` is one fixed vocabulary —
   the Conventional Commits types
   (feat|fix|docs|style|refactor|perf|test|build|ci|chore|revert) — and the
   prefix must match the PR title's type (CI enforces both). The prefix
   follows the *change type*, not the issue's `kind/*` label. The issue
   thread carries plan, status, and review artifacts.
2. **CI-first.** Every code change lands in the same PR as the CI that
   checks it. Red CI never merges. Run `make ci-gate` locally before
   pushing.
3. **Attribution.** Humans sign DCO (`git commit -s`); AI assistance is
   disclosed with `Assisted-by:` / `Generated-by:` trailers.
   `Co-Authored-By:` is banned for AI. AI never signs `Signed-off-by`.
4. **Never commit secrets.** Credentials, tokens, and private keys do not
   enter the repository, ever.
5. **Generated artifacts are read-only.** Anything produced by codegen or
   automation is regenerated, never hand-edited.
6. **Process authority.** If an external skill or methodology pack
   disagrees with this file on *process*, this file wins.

## Commands

- `make ci-gate` — run the full local governance gate (branch name, tool
  tests, trailer checks, shell lint). Rust checks live in the pre-commit
  hook and `.github/workflows/ci.yaml`.
- `make check-branch` — validate the current branch name against the
  naming rule before push (same rule as the `branch-name` CI check). Run
  `python3 .agents/tools/check_branch_name.py --title "feat: ..."` to also
  check prefix/title consistency.
- `make test-tools` — unit tests for `.agents/tools/`.
- `make check-trailers` — validate commit-message trailers on `HEAD`.
  Humans add `--dco` to require `Signed-off-by` on their range.
- One-time after clone: `git config core.hooksPath hooks` wires the
  committed `hooks/pre-commit` (governance checks + fmt/clippy/build) into
  every commit.

## Directory map

- `src/` — the Rust workspace runtime (see docs/ for architecture).
- `docs/` — long-form design documents; one file per design topic.
- `.agents/` — agent assets: decisions, governance tools, context, and
  the archived pre-issue task board. Rules: [.agents/AGENTS.md](.agents/AGENTS.md).
- `.rust-skills/` — vendored Rust methodology skills (git submodule).
- `.github/` — templates, workflows, labels.

## Collaboration protocol

- Work starts at the issue tracker: find or file the issue before writing
  any code, then cut the branch from it.
- Work happens in issues; plans are posted as issue comments before
  implementation for anything non-trivial.
- Acceptance criteria in the issue define "done"; PRs state how each is met.
- Multiple PRs may be in flight (feature and fix work proceed in parallel).
  Merges to main go through the merge queue: the queue validates each PR on
  top of everything ahead of it and merges only when green, so trunk stays
  releasable no matter how many PRs are concurrent.
- Once a PR is open, do not rebase or force-push it; push follow-up commits
  and let the squash merge clean up history (restarting CI and review churn
  costs more than tidy intermediate commits).
- Reviews judge conformance to this file as much as code quality.
- Bots and agents follow the same rules as humans: same CI, same trailer
  policy, same issue protocol.

## Versioning and releases

- Conventional Commits, enforced on PR titles (squash merge).
- Single-crate semver; version bumps land as `chore: bump version` commits.

---

# Project Constraints

## Analysis and Reasoning Style

- All analysis must be grounded in observable facts: logs, stack traces, code paths, compilation errors, or reproduced behavior.
- Do not use speculative qualifiers such as “maybe”, “possibly”, “likely”, “probably”, or “might” when explaining root causes or behavior.
- State what the code does, what the failure is, and what the evidence is. If the root cause is not yet proven, say so explicitly and list the next concrete verification step instead of guessing.

## Design-Time Skill Compliance

Skills are not post-hoc checklists. They must be read **before** writing code and used to shape the design.

- Identify the relevant skills for every change. At minimum:
  - Concurrency / shared state → [m07-concurrency](.rust-skills/skills/m07-concurrency/SKILL.md) and the "Rust Concurrency and Shared State" section below.
  - Error handling → [m06-error-handling](.rust-skills/skills/m06-error-handling/SKILL.md).
  - API / style → [coding-guidelines](.rust-skills/skills/coding-guidelines/SKILL.md).
- Turn the relevant skill constraints into concrete TodoList items before implementation.
- Use plan mode for any non-trivial change. The plan must explicitly state how each applicable skill constraint is satisfied.
- If the change affects concurrency, produce a lock-order table that lists every thread/loop and the order in which it acquires locks.

## Rust Concurrency and Shared State

When modifying code that uses threads, mutexes, channels, or event loops in this project:

- **`SharedState` has a single writer**. All mutations to `SharedState` must go through `StateEventLoop`. Do not mutate `SharedState` directly from `EventProxy`, client handlers, control commands, or any other thread.
- **`broadcast_snapshot` has a single caller**. Only `StateEventLoop` may call `broadcast_snapshot`. Other threads/loops that need a UI refresh must send a state event (e.g., `LocalSessionOutput`, `RemoteSessionOutput`, `RemoteSessionInputEcho`) and let `StateEventLoop` broadcast.
- **Lock order is part of the design**. Before adding or reordering locks, document the lock hierarchy in a code comment and verify that every code path acquires locks in the same order.
- **Never hold a lock while calling a callback, broadcasting, or doing I/O**. In particular:
  - `clients` lock must not be held while calling `build_snapshot` or `broadcast_snapshot`.
  - `sessions` lock must not be held while calling `broadcast_snapshot`.
  - `Term` lock must not be held while calling `broadcast_snapshot`.
- **Prefer message passing over shared mutable state**. Use channels to communicate between loops; keep mutexes for data wholly internal to one thread/loop.
- **Verify concurrency changes**. After touching concurrency code, run:
  - `cargo clippy -- -D warnings`
  - `cargo test --release ratatui`
  - `cargo test --release session_sync`
  - If the change affects lock-free paths, add or run a `loom` model test.

Reference skill: [actionbook/rust-skills m07-concurrency](.rust-skills/skills/m07-concurrency/SKILL.md).

## Rust Project Guidelines

For all Rust code in this project, follow [actionbook/rust-skills](.rust-skills/AGENTS.md) and [coding-guidelines](.rust-skills/skills/coding-guidelines/SKILL.md). The non-negotiable subset is:

### Error Handling

- Use `?` for propagation; do not silently `unwrap()`/`expect()` in library or runtime code.
- `expect()` is only for invariants that indicate a bug, not for user input or I/O failures.
- Prefer typed errors with `thiserror` for domain errors; use context chains where useful.

### Unsafe Code

- Every `unsafe` block must have a `// SAFETY:` comment explaining why it is sound.
- Keep `unsafe` blocks minimal; do not wrap large amounts of safe code in `unsafe`.
- Unsafe functions must document their safety contract in a `# Safety` section.

### Style and API Design

- Naming: `snake_case` for functions/variables, `CamelCase` for types/traits, `SCREAMING_SNAKE_CASE` for constants.
- No `get_` prefix on simple getters (`fn name()` not `fn get_name()`).
- Conversion methods: `as_` for cheap references, `to_` for expensive copies, `into_` for ownership-consuming conversions.
- Use newtypes (`struct Email(String)`) to enforce domain semantics at the type level.
- Prefer `&[T]` / `&str` over `&Vec<T>` / `&String` in public APIs.
- Pre-allocate collections when the size is known (`Vec::with_capacity`, `String::with_capacity`).

### Architecture

- Keep `main.rs` minimal; put logic in `lib.rs` or modules.
- Organize modules by feature, not by type.
- Use builders or typestates for complex construction with invariants.
- Prefer enums over boolean flags for mutually exclusive states.
- Public APIs must be documented (`///`); modules use `//!`.

### Modern Rust Defaults

- Use `std::sync::OnceLock` / `std::sync::LazyLock` instead of `lazy_static!`.
- Use the `?` operator instead of `try!()`.
- Run `cargo fmt --check` and `cargo clippy -- -D warnings` before committing.
