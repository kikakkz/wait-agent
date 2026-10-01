---
name: small-step-iteration
description: Decompose giant features into small PR-sized slices — small branches off latest main, one verifiable change per PR, quick merges
type: prompt
whenToUse: When starting, planning, or slicing a large feature, epic, or umbrella issue; when a branch lives longer than two days or a PR mixes refactor with feature work
---

# Small-step iteration for giant features

A giant feature (relay, WebUI, any multi-week capability) never lands as one
branch. It lands as a sequence of small slices, each merged to main while the
full test suite is green. Follow this skill whenever a task smells bigger than
a day or two of work.

## Principles

1. **One PR, one coherent change.** A reviewer (human or future-you) must be
   able to verify it in one sitting. Guideline: ≤ ~400 lines of diff, ≤ 2 days
   of work.
2. **Branch from latest main, merge back fast.** No long-lived feature
   branches. After each squash merge, cut the next slice from the new main.
3. **Existing tests stay green at every merge.** A slice that temporarily
   breaks the suite is not a slice — split further.
4. **Umbrella issues are checklists, not branches.** An epic issue tracks the
   slices; each slice gets its own issue before any code exists.
5. **Every slice has its own verification.** If you cannot state how a slice
   is verified independently, it is not sliced small enough.

## Slicing heuristics

- **Pure refactors first.** Extract the seam, rename, restructure — behavior
  byte-identical, existing tests as the safety net. New capability comes in
  later slices on top of the clean seam.
- **Codec/state machines before network I/O.** Anything testable on in-memory
  duplex pairs (frame codecs, state machines, parsers) ships before sockets.
- **Infrastructure before behavior.** Trait + one trivial implementation
  before the second real implementation.
- **Slice by verification boundary**, not by file or by layer.
- **Defer integration.** Two slices that only compose through a third piece
  of plumbing mean the plumbing is its own slice.

## Working loop (issue-driven, per AD-0001)

1. Find or file the issue for the *next slice only* (umbrella lists the rest).
2. Branch `<type>/<issue>-<slug>` from latest main; the prefix matches the PR
   title type (CI enforces).
3. Implement + verify per the issue's acceptance criteria.
4. Open PR with `Closes #N`; CI must be green; squash merge.
5. Repeat from the new main. If reality reshapes the plan, update the
   umbrella issue — the board shows truth, not intent.

## Red flags — stop and re-slice

- The branch is older than two days or has drifted from main.
- The PR mixes refactor + feature, or adds a transport + its first consumer.
- Tests are red "temporarily".
- The PR description needs more than a few lines to explain "what and why".
- You are about to write `#[ignore]`, `todo!()`, or a fallback to get CI green.
