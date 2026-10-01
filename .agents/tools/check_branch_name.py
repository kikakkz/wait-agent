#!/usr/bin/env python3
"""Validate the current branch name against the repository naming rule.

Rule (.agents/decisions/AD-0001, enforced on PRs by
.github/workflows/branch-name.yml):
branches are named `<type>/<issue>-<slug>` with the type from the fixed
Conventional Commits vocabulary, and the prefix must match the PR title's
type. Running this before push / before opening a PR catches a mismatch
while renaming the branch is still free — after a PR exists, a rename
forces close-and-reopen because GitHub cannot retarget a PR.

Checks:
  - branch format `<type>/<issue>-<slug>` (same regex as the workflow)
  - with --title, prefix vs PR-title type consistency
  - `main` and detached HEAD always pass: `main` is not a PR branch, and
    CI checks out PRs as detached HEAD where the workflow check applies.

Usage:
  check_branch_name.py [--branch NAME] [--title TITLE]

Defaults to the current git branch. Stdlib only. Exit 1 on any failure.
"""

import argparse
import re
import subprocess
import sys

TYPES = (
    "feat", "fix", "docs", "style", "refactor",
    "perf", "test", "build", "ci", "chore", "revert",
)

BRANCH_RE = re.compile(
    r"^(feat|fix|docs|style|refactor|perf|test|build|ci|chore|revert)"
    r"/[0-9]+-[a-z0-9-]+$"
)
TITLE_RE = re.compile(
    r"^(feat|fix|docs|style|refactor|perf|test|build|ci|chore|revert)"
    r"(\([a-z0-9-]+\))?!?: .+"
)
MAIN_BRANCHES = frozenset({"main"})


def current_branch():
    out = subprocess.run(
        ["git", "rev-parse", "--abbrev-ref", "HEAD"],
        capture_output=True, text=True, check=True,
    )
    return out.stdout.strip()


def check(branch, title=None):
    """Return a list of error strings; empty means the branch is OK."""
    errors = []
    if branch in MAIN_BRANCHES:
        return errors
    m = BRANCH_RE.match(branch)
    if not m:
        errors.append(
            f"branch '{branch}' must be '<type>/<issue>-<slug>' "
            f"(type one of: {' '.join(TYPES)})"
        )
        return errors
    if title is None:
        return errors
    tm = TITLE_RE.match(title)
    if not tm:
        errors.append(f"title '{title}' must be Conventional Commits format")
    elif tm.group(1) != m.group(1):
        errors.append(
            f"mismatch: branch prefix '{m.group(1)}/' vs title type "
            f"'{tm.group(1)}:' — rename the branch or fix the title "
            f"before opening a PR"
        )
    return errors


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--branch", help="branch to check (default: current)")
    parser.add_argument("--title", help="intended PR title (optional)")
    args = parser.parse_args(argv)

    try:
        branch = args.branch if args.branch is not None else current_branch()
    except subprocess.CalledProcessError:
        print("ERROR: not inside a git repository.", file=sys.stderr)
        return 1

    if branch == "HEAD":
        print("detached HEAD; branch-name check skipped "
              "(CI enforces it on PR events)")
        return 0

    errors = check(branch, args.title)
    if errors:
        for e in errors:
            print(f"ERROR: {e}", file=sys.stderr)
        return 1
    print(f"branch-name: OK ('{branch}')")
    return 0


if __name__ == "__main__":
    sys.exit(main())
