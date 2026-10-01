#!/usr/bin/env python3
"""Unit tests for check-trailers.sh against throwaway git repositories."""

import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parent.parent / "check-trailers.sh"


def run_script(repo: Path, *args: str) -> subprocess.CompletedProcess:
    env = os.environ.copy()
    env["GIT_CONFIG_GLOBAL"] = "/dev/null"  # isolate from user git config
    env["GIT_CONFIG_SYSTEM"] = "/dev/null"
    return subprocess.run(
        ["bash", str(SCRIPT), *args],
        cwd=repo, env=env, capture_output=True, text=True,
    )


class CheckTrailersTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = Path(tempfile.mkdtemp())
        self.repo = self.tmp / "repo"
        self.repo.mkdir()
        self.env = os.environ.copy()
        self.env["GIT_CONFIG_GLOBAL"] = "/dev/null"
        self.env["GIT_CONFIG_SYSTEM"] = "/dev/null"
        self.git("init", "-q", "-b", "main")
        self.git("config", "user.name", "T")
        self.git("config", "user.email", "t@example.com")

    def tearDown(self) -> None:
        shutil.rmtree(self.tmp, ignore_errors=True)

    def git(self, *args: str) -> None:
        subprocess.run(["git", *args], cwd=self.repo, env=self.env,
                       check=True, capture_output=True)

    def commit(self, message: str) -> None:
        (self.repo / "f.txt").write_text(message, encoding="utf-8")
        self.git("add", "-A")
        self.git("commit", "-q", "-m", message)

    def test_signed_commit_passes_with_dco(self) -> None:
        self.commit("fix: x\n\nbody line\n\nSigned-off-by: T <t@example.com>")
        r = run_script(self.repo, "--dco", "HEAD")
        self.assertEqual(r.returncode, 0, r.stderr)

    def test_unsigned_commit_fails_with_dco(self) -> None:
        self.commit("fix: x")
        r = run_script(self.repo, "--dco", "HEAD")
        self.assertEqual(r.returncode, 1)

    def test_unsigned_commit_passes_without_dco(self) -> None:
        self.commit("fix: x")
        r = run_script(self.repo, "HEAD")
        self.assertEqual(r.returncode, 0, r.stderr)

    def test_co_authored_by_banned(self) -> None:
        self.commit("fix: x\n\nCo-Authored-By: bot <bot@example.com>")
        r = run_script(self.repo, "HEAD")
        self.assertEqual(r.returncode, 1)

    def test_multiline_signed_body_passes_once(self) -> None:
        # Regression: body lines must not be treated as separate commits.
        msg = ("fix: x\n\n- bullet one\n- bullet two\n- bullet three\n\n"
               "Assisted-by: tool\n\nSigned-off-by: T <t@example.com>")
        self.commit(msg)
        r = run_script(self.repo, "--dco", "HEAD")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(r.stderr.count("missing Signed-off-by"), 0)

    def test_malformed_ai_trailer_fails(self) -> None:
        self.commit("fix: x\n\nassisted by: vague wording")
        r = run_script(self.repo, "HEAD")
        self.assertEqual(r.returncode, 1)

    def test_merge_commit_without_signoff_flags_commit_sha(self) -> None:
        # Merge commits carry no trailers; when in range they must be
        # flagged once, naming the merge commit.
        self.commit("fix: a\n\nSigned-off-by: T <t@example.com>")
        self.git("checkout", "-q", "-b", "feature")
        self.commit("feat: b\n\nSigned-off-by: T <t@example.com>")
        self.git("checkout", "-q", "main")
        self.git("merge", "--no-ff", "-m", "Merge b into main", "feature")
        r = run_script(self.repo, "--dco", "main")
        self.assertEqual(r.returncode, 1)
        merge_sha = subprocess.run(
            ["git", "rev-list", "--merges", "-1", "main"],
            cwd=self.repo, env=self.env, capture_output=True, text=True,
            check=True).stdout.strip()
        self.assertIn(merge_sha[:7], r.stderr)


if __name__ == "__main__":
    unittest.main()
