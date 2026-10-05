<!--
Issue-driven development: every PR must reference an issue.
Title must be Conventional Commits format (CI enforces).
-->

## Issue

Closes #

## What and why

## How acceptance criteria are met

## Acceptance (验收)

<!-- Required when the PR adds a user-reachable surface (CLI flag, config
     key, protocol message/rpc); enforced by .agents/tools/check_surface_e2e.py
     (issue #136). Delete this section only when no surface changed. -->

- Proof command(s) — exact commands a reviewer can run to see it work:
- Executing CI layer — the job (or e2e script) that runs them:
- Exemption (only if there is no e2e): surface id(s) + reason registered
  in `.agents/tools/surface_e2e_exemptions.txt`:

## Checklist

- [ ] An issue exists for this change (filed first if missing); the branch
      is named `<type>/<issue>-<slug>` with the matching title type
- [ ] CI added/updated in this PR for every code change (CI-first rule)
- [ ] PR title is Conventional Commits format
- [ ] DCO sign-off present on every commit (`git commit -s`); the human
      operator signs for AI-assisted commits too, and AI contributions add
      `Assisted-by:` / `Generated-by:`, never `Co-Authored-By:`
- [ ] AGENTS.md updated if conventions changed
