---
name: session-bootstrap
description: Session and task bootstrap protocol for this repository. Use at the start of any new session, when picking up a new task, or when resuming work after context compaction — rebuilds working context from the repo and the issue tracker instead of trusting session memory.
type: prompt
whenToUse: At the start of a new session in this repository, when starting any new task, or after context compaction mid-task
---

# Session bootstrap: rebuild context from sources, not from memory

Session memory decays and compacts; the repo and the issue tracker do
not. Follow this protocol at the start of every session and every task.

## 1. Load the contract and stable context

- `AGENTS.md` is injected automatically — it is the operating contract.
- Read the files in `.agents/index.yaml` `read_order`: context files and
  `.agents/decisions/README.md`. Skim AD bodies for decisions that touch
  your task.

## 2. Verify freshness anchors before trusting them

Context files describe facts that rot. Before acting on one, check its
anchor: does the path exist, does the issue number resolve, does the phase
name match the current milestone?

- Stale entry found → do NOT silently follow it; file or note a
  `kind/cleanup` issue and work from ground truth (code, tracker).
- The PR that changes an underlying fact must update the corresponding
  context file in the same change (`.agents/AGENTS.md`, `context/` rules).

## 3. Pick the task from the tracker, not from memory

- Task state lives in GitHub issues (AD-0001). Open the "relay phase 1"
  milestone (or the user-specified issue) and read the issue **in full**,
  including comments and its acceptance criteria.
- Never continue "the previous session's task" from memory — re-read its
  issue. If the issue contradicts your memory, the issue wins.

## 4. Anchor the task before writing code

- Non-trivial work: post the plan as an issue comment first (AGENTS.md
  collaboration protocol).
- Branch `<type>/<issue>-<slug>` from the **latest origin/main**, prefix
  matching the intended PR title type.
- Turn the acceptance criteria into a TodoList; invoke
  `planning-and-task-breakdown` / `small-step-iteration` when the task is
  bigger than a day of work.

## 5. Spend context deliberately

- Broad codebase investigation → delegate to an explore subagent; do not
  paste large file dumps into this context.
- Waiting on CI → use `.agents/tools/pr_watch.py` (it polls outside the
  model context and reports only actionable states).
- Methodology questions → invoke the relevant skill instead of
  re-deriving practice from first principles.

## 6. Hand the loop over when the PR opens

Once a PR exists, `.agents/skills/pr-watch` owns the review-fix loop;
this session triages what pr-watch surfaces instead of polling manually.
