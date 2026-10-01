---
name: pr-watch
description: Own the PR review-fix loop end to end. Use when a PR is open and anything blocks it — failing checks, review findings, undecided state. Watches, reports failures in an actionable form, fixes on-branch with attribution trailers, and hands in (merge/queue) exactly once per head when green.
type: prompt
whenToUse: When a PR of this repository is open and you are responsible for getting it merged — CI red, review comments arrived, or it sits green but unmerged
---

# pr-watch: own the review-fix loop

Adapted from kikakkz/looming#23. The mechanical loop around a PR —
watch, triage, fix, push, hand in — is agent work. The judgment calls
stay explicit. See #60 for the acceptance contract.

## Division of labor

- **The tool** (`.agents/tools/pr_watch.py`) observes and reports: required
  checks per head SHA, review decisions, hand-in state. It never guesses at
  fixes. Run it with `--once` for a single observation, or as a loop; it
  exits (and prints a JSON report) the moment anything needs an agent:
  failing checks, new changes-requested, merged, or handed-in state.
- **The agent** triages each finding and fixes on-branch:
  - classify per repo convention: codified rule (`AGENTS.md`,
    `.agents/context/constraints.yaml`) / documented craft (skills) /
    **accepted residual** — the last one requires an explicit reply comment
    on the thread, never silence;
  - skip resolved or stale threads;
  - root-cause per the project constraints: no fallback or silent-drop fixes;
  - run `make ci-gate` locally before pushing; when Rust changed, also the
    focused `cargo test` from the issue's verification section.

## On-branch fix discipline

- Follow-up commits on the SAME branch — never open a replacement PR,
  never rename the branch, never rebase or force-push an open PR (AGENTS.md
  collaboration protocol; squash cleans history at land time).
- Every fix commit carries the attribution trailer (`Generated-by:` for
  machine-led work); AI never signs `Signed-off-by:`.
- After pushing, run `pr_watch.py --once` again — the head SHA changed, so
  hand-in eligibility is re-evaluated from scratch.

## Hand-in

Exactly once per head SHA, and only when: all required checks green, no
pending changes-requested newer than the head commit, PR still open. The
tool enforces the once-per-head invariant with a local state file; the
hand-in itself is `PUT /pulls/{n}/merge` (squash) — with a merge queue
enabled this enqueues, without it merges directly.

## Stop conditions

1. PR merged — done; report the squash SHA to the issue.
2. Genuinely new changes-requested — stop, summarize findings to the user.
3. User interrupt — always honored; state file keeps hand-in memory.

## Never do

- Hand in twice for the same head SHA.
- Fix by disabling a check, `#[ignore]`-ing a test, or widening a gate.
- Merge with red required checks "because it is probably fine".
- Leave the loop without a trail: every fix round ends with either a push
  or a written reason in the session/issue.
