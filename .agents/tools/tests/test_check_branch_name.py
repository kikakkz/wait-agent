#!/usr/bin/env python3
"""Unit tests for check_branch_name.py."""

import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

import check_branch_name


class CheckBranchTests(unittest.TestCase):
    def test_compliant_branch_ok(self):
        for branch in ("ci/9-control-plane-cleanup",
                       "feat/6-memory-housekeeping",
                       "fix/1-bootstrap-corrections",
                       "docs/20-adr-layout"):
            with self.subTest(branch=branch):
                self.assertEqual(check_branch_name.check(branch), [])

    def test_main_always_ok(self):
        self.assertEqual(check_branch_name.check("main"), [])
        self.assertEqual(check_branch_name.check("main", "ci: x"), [])

    def test_malformed_branches_rejected(self):
        bad = (
            "1-bootstrap-corrections",   # no type
            "feature/9-coderabbit",      # type outside the vocabulary
            "feat/9_coderabbit",         # underscore in slug
            "feat/9-Coderabbit",         # uppercase in slug
            "feat/9-",                   # empty slug
            "feat/nine-coderabbit",      # issue must be numeric
            "feat/9/coderabbit",         # wrong separator
            "CI/9-coderabbit",           # uppercase type
            "feat/9 coderabbit",         # space in slug
        )
        for branch in bad:
            with self.subTest(branch=branch):
                errors = check_branch_name.check(branch)
                self.assertEqual(len(errors), 1, errors)
                self.assertIn("must be '<type>/<issue>-<slug>'", errors[0])

    def test_title_mismatch_flagged(self):
        # The classic failure: feat/ branch, ci: title.
        errors = check_branch_name.check(
            "feat/9-coderabbit-review-gate",
            "ci: add CodeRabbit configuration as the review gate",
        )
        self.assertEqual(len(errors), 1, errors)
        self.assertIn("mismatch", errors[0])

    def test_title_match_ok(self):
        self.assertEqual(
            check_branch_name.check(
                "ci/9-coderabbit-review-gate",
                "ci: add CodeRabbit configuration as the review gate",
            ),
            [],
        )

    def test_malformed_title_flagged(self):
        errors = check_branch_name.check("ci/9-coderabbit", "not conventional")
        self.assertEqual(len(errors), 1, errors)
        self.assertIn("Conventional Commits", errors[0])

    def test_malformed_title_does_not_mask_branch(self):
        errors = check_branch_name.check("not-a-branch", "bad title")
        self.assertEqual(len(errors), 1)
        self.assertIn("must be '<type>/<issue>-<slug>'", errors[0])


class CliTests(unittest.TestCase):
    def test_explicit_branch_flag(self):
        self.assertEqual(
            check_branch_name.main(["--branch", "ci/9-x", "--title", "ci: x"]),
            0,
        )

    def test_explicit_branch_failure(self):
        self.assertEqual(check_branch_name.main(["--branch", "oops"]), 1)

    def test_empty_branch_rejected(self):
        # '' is a supplied (malformed) value, not an omitted argument.
        self.assertEqual(check_branch_name.main(["--branch", ""]), 1)


if __name__ == "__main__":
    unittest.main()
