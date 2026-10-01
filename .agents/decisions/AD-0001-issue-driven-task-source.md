# AD-0001: Task source moves from the local task board to GitHub issues

- status: accepted
- date: 2026-10-01

## Context

Task state lived in a bespoke local control plane (`.agents/tasks/`:
`backlog.yaml` plus ~169 queued YAML cards, mirrored into primitives,
runbooks, schemas, and state files). Maintaining it required a four-place
update protocol per status change; the board had drifted from reality
(e.g. bug cards still `ready` long after the backlog notes recorded them
fixed; card scopes diverging from their governing design doc). Review of
the sibling project `kikakkz/looming` showed an issue-driven model where
GitHub issues are the single task source, enforced by CI (branch name and
PR title checks), with agent/human contributors following one protocol.

## Decision

1. GitHub issues become the single task source of truth. No issue, no
   code; every PR references `Closes #N`.
2. Branches are named `<type>/<issue>-<slug>` from the fixed Conventional
   Commits vocabulary; the prefix must match the PR title type, enforced
   by `.github/workflows/branch-name.yml` and `pr-title.yml`.
3. The local board freezes as read-only history under `.agents/archive/`
   (nothing deleted; cards remain valid archaeology).
4. Humans sign DCO (`git commit -s`); AI contributions disclose with
   `Assisted-by:` / `Generated-by:` trailers; `Co-Authored-By:` is banned
   for AI.
5. Card structure (context, design ref, dependencies, acceptance
   criteria, verification) moves into the GitHub issue templates, so task
   quality is preserved, not just relocated.

## Consequences

- Task state is single-write (the issue), reachable by any agent or human
  with tracker access; the four-mirror protocol is retired.
- Open work at migration time was re-filed as issues (relay phase 1
  milestone, cloud-auth backlog, open bugs). Done/superseded history was
  not re-filed.
- The pre-commit hook and `make ci-gate` gain governance checks ported
  from looming (branch name, trailers, tool tests).
- `.agents/` keeps its non-task roles: decision records (`decisions/`),
  governance tools (`tools/`), stable context (`context/`), frozen history
  (`archive/`).

## Options rejected

- Keep the local board and mirror it to issues: preserves the
  four-mirror drift problem in a new shape.
- Delete the old board instead of archiving: destroys archaeology for no
  benefit; a read-only archive costs nothing.
