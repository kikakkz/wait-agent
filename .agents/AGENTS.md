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

## `archive/` — frozen history

- The pre-issue task board (`tasks/` with backlog + queued cards,
  `primitives/`, `runbooks/`, `schemas/`, `state/`), kept read-only for
  archaeology. Do not add new material here and do not extend its files.

## General

- Never store secrets in this directory.
- Changes to this directory follow the same issue-driven flow as code.
