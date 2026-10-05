#!/usr/bin/env python3
"""Reject new user-reachable surfaces that ship without process-level
acceptance (issue #136).

Background: two merged slices (#129, #131 slice 2) exposed a process
hole — a config or protocol surface could land with a design doc and
unit tests but no process-level proof that a real binary exposes it.
This script is the mechanical half of the fix (AGENTS.md hard
constraint): a diff that adds a user-reachable surface must either
touch scripts/e2e/ in the same diff or register an explicit exemption.

Heuristics (added-line based, tuned to this codebase's idioms — keep
them few and precise, prefer misses over false positives):

  CLI flags   — added match arms on long-option literals
                (`"--new-flag" => ...`, also `"--a" | "--b" =>`) in
                src/cli.rs or src/cli/. The repo's CLI parser is
                hand-rolled, so new user flags always appear as new
                match arms; `--help`/`--version` arms are ignored as
                universal. Reconstructed args (to_cli_args pushes) and
                test argv arrays do not match the arm shape, so they
                do not trip the rule.
  Config keys — added match arms on key literals (`"new_key" => ...`)
                in the hand-rolled TOML stores (files whose name
                contains `toml_store`) or src/web/config.rs. Those
                parsers reject unknown keys, so a new accepted key is
                always a new arm.
  Protocol    — added `rpc Name(...)` or `message Name` lines in
                proto/**/*.proto.

Coverage proxy: the same diff touching anything under scripts/e2e/
counts as acceptance (the e2e jobs in CI execute that directory).
Exemptions: .agents/tools/surface_e2e_exemptions.txt, one surface per
line as `<surface-id> <reason>` (`#` comments and blank lines ignored;
a surface id without a reason is a hard error). Surface ids are the
flag literal (`--new-flag`), `key:<config-key>`, or `proto:<MessageName>`.

Usage:
  check_surface_e2e.py [--range A..B] [--repo DIR] [--exemptions FILE]

Defaults: HEAD vs its first parent, the current repository, and
<repo>/.agents/tools/surface_e2e_exemptions.txt. The pre-commit hook
runs the default (HEAD commit); CI runs --range base..head over the
whole PR. Stdlib only, read-only git access.

Exit codes: 0 = pass, 1 = policy violation or malformed exemption
file, 2 = operational error (not a git repository, bad revision).
"""

import argparse
import re
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path

EMPTY_TREE = "4b825dc642cb6eb9a060e54bf8d69288fbee4904"
E2E_PATHSPEC = "scripts/e2e/"
DEFAULT_EXEMPTIONS = ".agents/tools/surface_e2e_exemptions.txt"

CLI_PATHS = ("src/cli.rs", "src/cli/")
CONFIG_FILE_HINTS = ("toml_store",)
CONFIG_EXACT_FILES = frozenset({"src/web/config.rs"})
PROTO_PATH = "proto/"

# Added-line shapes, minus the leading '+' (diff -U0 has no context).
CLI_ARM_RE = re.compile(r'^\s*"(--[a-z0-9][a-z0-9-]*)"\s*(?:=>|\|)')
CONFIG_ARM_RE = re.compile(r'^\s*"([a-z][a-z0-9_]*)"\s*=>')
PROTO_DECL_RE = re.compile(r"^\s*(?:rpc|message)\s+([A-Za-z_][A-Za-z0-9_]*)")
UNIVERSAL_FLAGS = frozenset({"--help", "--version"})

HUNK_RE = re.compile(r"^@@ -\d+(?:,\d+)? \+(\d+)(?:,\d+)? @@")


@dataclass
class Hit:
    """A newly added user-reachable surface in the diff."""

    kind: str
    surface_id: str
    path: str
    line: int
    text: str


def run_git(repo, args):
    return subprocess.run(
        ["git", "-C", repo, *args],
        capture_output=True,
        text=True,
        check=True,
    ).stdout


def resolve_rev(repo, rev):
    out = subprocess.run(
        ["git", "-C", repo, "rev-parse", "--verify", f"{rev}^{{commit}}"],
        capture_output=True,
        text=True,
    )
    if out.returncode != 0:
        raise ValueError(f"cannot resolve revision '{rev}': {out.stderr.strip()}")
    return out.stdout.strip()


def default_range(repo):
    """(parent_of_HEAD, HEAD); the empty tree stands in for a root parent."""
    head = resolve_rev(repo, "HEAD")
    parent = subprocess.run(
        ["git", "-C", repo, "rev-parse", "--verify", "HEAD^"],
        capture_output=True,
        text=True,
    )
    if parent.returncode != 0:
        return EMPTY_TREE, head
    return parent.stdout.strip(), head


def diff_unified0(repo, rev_a, rev_b):
    return run_git(repo, ["diff", "--unified=0", rev_a, rev_b])


def e2e_touched(repo, rev_a, rev_b):
    out = run_git(
        repo, ["diff", "--name-only", rev_a, rev_b, "--", E2E_PATHSPEC]
    )
    return bool(out.strip())


def added_lines_per_file(diff_text):
    """Split a `git diff -U0` into {path: [(new_line_number, text)]}.

    Only added lines are collected, with their 1-based line number in
    the post-image file (tracked through the hunk headers). The current
    file is taken from the `+++ b/...` post-image header, so renames
    and deletions (`+++ /dev/null`) resolve naturally.
    """
    files = {}
    current_path = None
    new_line = 0
    for raw in diff_text.splitlines():
        if raw.startswith("+++ "):
            target = raw[len("+++ "):]
            if target == "/dev/null":
                current_path = None
            else:
                current_path = (
                    target[len("b/"):] if target.startswith("b/") else target
                )
                files.setdefault(current_path, [])
            continue
        if current_path is None:
            continue
        hunk = HUNK_RE.match(raw)
        if hunk:
            new_line = int(hunk.group(1))
            continue
        if raw.startswith("+"):
            files[current_path].append((new_line, raw[1:]))
            new_line += 1
        elif raw.startswith("-"):
            continue  # removed lines do not consume post-image numbers
        else:
            new_line += 1  # header/context line
    return files


def is_config_file(path):
    if path in CONFIG_EXACT_FILES:
        return True
    name = path.rsplit("/", 1)[-1]
    return path.startswith("src/") and any(h in name for h in CONFIG_FILE_HINTS)


def detect_surfaces(files):
    """Apply the three heuristics to {path: [(line_no, added_text)]}."""
    hits = []
    for path, lines in files.items():
        for line_no, text in lines:
            if path.startswith(CLI_PATHS):
                arm = CLI_ARM_RE.match(text)
                if arm and arm.group(1) not in UNIVERSAL_FLAGS:
                    hits.append(
                        Hit("CLI flag", arm.group(1), path, line_no, text.strip())
                    )
                continue
            if is_config_file(path):
                arm = CONFIG_ARM_RE.match(text)
                if arm:
                    hits.append(
                        Hit(
                            "config key",
                            f"key:{arm.group(1)}",
                            path,
                            line_no,
                            text.strip(),
                        )
                    )
                continue
            if path.startswith(PROTO_PATH) and path.endswith(".proto"):
                decl = PROTO_DECL_RE.match(text)
                if decl:
                    hits.append(
                        Hit(
                            "protocol surface",
                            f"proto:{decl.group(1)}",
                            path,
                            line_no,
                            text.strip(),
                        )
                    )
    return hits


def parse_exemptions(text):
    """Parse the exemption file.

    Returns (exemptions, errors): exemptions maps surface id to reason;
    errors lists malformed lines (an id with no reason). Blank lines and
    `#` comment lines are ignored.
    """
    exemptions = {}
    errors = []
    for lineno, raw in enumerate(text.splitlines(), start=1):
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        surface_id, _, reason = line.partition(" ")
        reason = reason.strip()
        if reason.startswith("#") or not reason:
            errors.append(
                f"exemption line {lineno}: '{surface_id}' has no reason; "
                f"use '<surface-id> <reason>'"
            )
            continue
        exemptions[surface_id] = reason
    return exemptions, errors


def load_exemptions(path):
    if not path.is_file():
        return {}, []
    return parse_exemptions(path.read_text(encoding="utf-8"))


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--range",
        metavar="A..B",
        help="revision range to check (default: HEAD^..HEAD, empty tree as "
        "parent for a root commit)",
    )
    parser.add_argument("--repo", default=".", help="repository root")
    parser.add_argument(
        "--exemptions",
        help="exemption file (default: <repo>/" + DEFAULT_EXEMPTIONS + ")",
    )
    args = parser.parse_args(argv)

    repo = args.repo
    try:
        subprocess.run(
            ["git", "-C", repo, "rev-parse", "--is-inside-work-tree"],
            capture_output=True,
            check=True,
        )
    except (subprocess.CalledProcessError, FileNotFoundError):
        print("ERROR: not inside a git repository.", file=sys.stderr)
        return 2

    try:
        if args.range:
            rev_a, _, rev_b = args.range.partition("..")
            if not rev_a or not rev_b or ".." in rev_b:
                print(
                    f"ERROR: --range must be 'A..B' (got '{args.range}').",
                    file=sys.stderr,
                )
                return 2
            rev_a = resolve_rev(repo, rev_a) if rev_a != EMPTY_TREE else rev_a
            rev_b = resolve_rev(repo, rev_b)
        else:
            rev_a, rev_b = default_range(repo)
    except ValueError as error:
        print(f"ERROR: {error}", file=sys.stderr)
        return 2

    exemptions_path = (
        Path(args.exemptions)
        if args.exemptions
        else Path(repo) / DEFAULT_EXEMPTIONS
    )
    exemptions, errors = load_exemptions(exemptions_path)
    if errors:
        for error in errors:
            print(f"ERROR [{exemptions_path}]: {error}", file=sys.stderr)
        return 1

    hits = detect_surfaces(added_lines_per_file(diff_unified0(repo, rev_a, rev_b)))
    if not hits:
        print(f"surface-e2e: OK (no new surfaces in {rev_a[:9]}..{rev_b[:9]})")
        return 0

    # The same flag often lands in several subcommand parsers; report each
    # surface once, at its first occurrence.
    unique = {}
    for hit in hits:
        unique.setdefault(hit.surface_id, hit)
    uncovered = [h for sid, h in unique.items() if sid not in exemptions]
    if not uncovered:
        print(
            f"surface-e2e: OK ({len(hits)} new surface(s), all exempted: "
            f"{', '.join(sorted(h.surface_id for h in hits))})"
        )
        return 0

    covered = e2e_touched(repo, rev_a, rev_b)
    if covered:
        print(
            f"surface-e2e: OK ({len(uncovered)} new surface(s) accepted by "
            f"scripts/e2e changes in the same diff: "
            f"{', '.join(sorted(h.surface_id for h in uncovered))})"
        )
        return 0

    for hit in uncovered:
        print(
            f"ERROR: new {hit.kind} `{hit.surface_id}` at {hit.path}:{hit.line} "
            f"(`{hit.text}`) has no process-level acceptance",
            file=sys.stderr,
        )
    print(
        "New user-reachable surfaces require process-level acceptance in the "
        "same change (issue #136 / AGENTS.md):",
        file=sys.stderr,
    )
    print(
        "  1. add or extend a case under scripts/e2e/ (executed by the "
        "e2e-relay CI job), or",
        file=sys.stderr,
    )
    print(
        f"  2. register an exemption '<surface-id> <reason>' in "
        f"{DEFAULT_EXEMPTIONS}.",
        file=sys.stderr,
    )
    return 1


if __name__ == "__main__":
    sys.exit(main())
