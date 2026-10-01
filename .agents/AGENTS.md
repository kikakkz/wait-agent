# AGENTS.md — `.agents/`

Agent assets live here. Task state does **not** live here: since
`.agents/decisions/AD-0001` the task source of truth is GitHub issues, and
the old local task board is frozen under `archive/`.

## `decisions/` — decision records (ADRs)

- One file per architecture decision (`AD-NNNN-slug.md`), immutable except
  status flips (`accepted` ↔ `superseded`).
- `README.md` is the index; keep it in sync when adding a record.
- Record *decisions*, not narratives — the why, the options rejected, and
  the consequences. Link design docs for the full detail.

## `tools/` — repository governance scripts

- Every script ships with unit tests in `tests/` in the same change.
- Bash scripts must pass `shellcheck`; Python must be stdlib-only.
- Scripts must be deterministic and side-effect free outside their args.

## `context/` — stable project context

- `project.yaml`, `constraints.yaml`, `repo-map.yaml`: slow-moving facts
  an agent needs before exploring the repo. Update when they change,
  never per-task.
- **Freshness is a rule, not a hope**: the PR that changes an underlying
  fact updates the corresponding context file in the same change. The
  `session-bootstrap` skill verifies anchors (paths, issue numbers, phase
  names) before trusting them; stale entries are reported, never silently
  followed.

## `archive/` — frozen history

- The pre-issue task board (`tasks/` with backlog + queued cards,
  `primitives/`, `runbooks/`, `schemas/`, `state/`), kept read-only for
  archaeology. Do not add new material here and do not extend its files.

## `skills/` — agent skills

- `small-step-iteration/` is repo-authored glue (branch sizing, PR
  discipline) and may be edited like any repo file.
- Vendored skills (upstream name directories, e.g. `incremental-implementation/`)
  carry a provenance header: upstream repo, revision sha, license. Do not
  edit their content; update by re-vendoring a new revision.
- Upstream skills that mention local task files defer to this repo's
  contract: the task tracker is GitHub issues (AD-0001), so plan/task
  artifacts live in issues, never in `tasks/plan.md`-style local files.

## General

- Never store secrets in this directory.
- Changes to this directory follow the same issue-driven flow as code.
