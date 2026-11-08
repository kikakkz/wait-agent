#!/usr/bin/env python3
"""Unit tests for pr_watch.py against a scripted fake API."""

import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
os.environ["PR_WATCH_STATE_DIR"] = tempfile.mkdtemp(prefix="pr-watch-test-")

import pr_watch


class StubApi:
    def __init__(self):
        self.comments = []

    def post(self, path, payload):
        self.comments.append((path, payload))
        return {}, 201


class FakeApi:
    """Scripted API: paths return queued values in order."""

    def __init__(self, script):
        self.script = script
        self.puts = []

    def get(self, path):
        if path not in self.script:
            raise AssertionError(f"unexpected GET {path}")
        value = self.script[path]
        if isinstance(value, list):
            if not value:
                raise AssertionError(f"GET {path} exhausted")
            return value.pop(0)
        return value

    def put(self, path, payload):
        self.puts.append((path, payload))
        value = self.script.get(path + " #PUT")
        if isinstance(value, list):
            if not value:
                raise AssertionError(f"PUT {path} exhausted")
            return value.pop(0)
        return value or ({"merged": True}, 200)


SHA = "abc123"
PR_OPEN = {"merged": False, "state": "open",
           "head": {"sha": SHA, "ref": "feat/1-x"},
           "base": {"ref": "main"}, "html_url": "https://example.test/pr/1"}
PROTECTION = {"required_status_checks": {"contexts": ["check", "test"]}}
CHECKS_PENDING = {"check_runs": [
    {"name": "check", "status": "in_progress", "conclusion": None,
     "html_url": "u1"},
    {"name": "test", "status": "queued", "conclusion": None, "html_url": "u2"},
]}
CHECKS_GREEN = {"check_runs": [
    {"name": "check", "status": "completed", "conclusion": "success",
     "html_url": "u1"},
    {"name": "test", "status": "completed", "conclusion": "success",
     "html_url": "u2"},
]}
CHECKS_RED = {"check_runs": [
    {"name": "check", "status": "completed", "conclusion": "failure",
     "html_url": "u1"},
    {"name": "test", "status": "completed", "conclusion": "success",
     "html_url": "u2"},
]}
COMMITS = [{"commit": {"committer": {"date": "2026-10-01T10:00:00Z"}}}]
NO_REVIEWS = []


class PrWatchTests(unittest.TestCase):
    def setUp(self):
        state_dir = Path(os.environ["PR_WATCH_STATE_DIR"])
        for f in state_dir.glob("*.json"):
            f.unlink()

    def api_green(self):
        return FakeApi({
            "/pulls/1": [json.loads(json.dumps(PR_OPEN))],
            "/branches/main/protection": [PROTECTION],
            f"/commits/{SHA}/check-runs?per_page=100": [CHECKS_GREEN],
            "/pulls/1/commits?per_page=100": [COMMITS],
            "/pulls/1/reviews?per_page=100": [NO_REVIEWS],
        })

    def test_pending_when_checks_incomplete(self):
        api = FakeApi({
            "/pulls/1": [json.loads(json.dumps(PR_OPEN))],
            "/branches/main/protection": [PROTECTION],
            f"/commits/{SHA}/check-runs?per_page=100": [CHECKS_PENDING],
        })
        report = pr_watch.run_once(api, "o/r", 1)
        self.assertEqual(report["status"], "checks_pending")
        self.assertIn("check", report["pending"])
        self.assertIn("test", report["pending"])

    def test_missing_required_check_is_failure(self):
        checks = {"check_runs": [
            {"name": "check", "status": "completed", "conclusion": "success",
             "html_url": "u1"},
        ]}
        api = FakeApi({
            "/pulls/1": [json.loads(json.dumps(PR_OPEN))],
            "/branches/main/protection": [PROTECTION],
            f"/commits/{SHA}/check-runs?per_page=100": [checks],
        })
        report = pr_watch.run_once(api, "o/r", 1)
        self.assertEqual(report["status"], "failing_checks")
        self.assertEqual(report["failing"]["test"]["status"], "missing")

    def test_failing_checks_report_only_bad_ones(self):
        api = FakeApi({
            "/pulls/1": [json.loads(json.dumps(PR_OPEN))],
            "/branches/main/protection": [PROTECTION],
            f"/commits/{SHA}/check-runs?per_page=100": [CHECKS_RED],
        })
        report = pr_watch.run_once(api, "o/r", 1)
        self.assertEqual(report["status"], "failing_checks")
        self.assertEqual(list(report["failing"]), ["check"])

    def test_hand_in_once_per_head(self):
        api = self.api_green()
        first = pr_watch.run_once(api, "o/r", 1)
        self.assertEqual(first["status"], "handed_in")
        path, payload = api.puts[0]
        self.assertEqual(path, "/pulls/1/merge")
        self.assertEqual(payload["merge_method"], "squash")
        self.assertEqual(payload["expected_head_sha"], SHA)

        api2 = self.api_green()
        second = pr_watch.run_once(api2, "o/r", 1)
        self.assertEqual(second["status"], "handed_in_waiting")
        self.assertEqual(api2.puts, [])

    def test_new_head_reenables_hand_in(self):
        pr_watch.run_once(self.api_green(), "o/r", 1)
        new_sha = "def456"
        pr2 = json.loads(json.dumps(PR_OPEN))
        pr2["head"]["sha"] = new_sha
        api = FakeApi({
            "/pulls/1": [pr2],
            "/branches/main/protection": [PROTECTION],
            f"/commits/{new_sha}/check-runs?per_page=100": [CHECKS_GREEN],
            "/pulls/1/commits?per_page=100": [COMMITS],
            "/pulls/1/reviews?per_page=100": [NO_REVIEWS],
        })
        report = pr_watch.run_once(api, "o/r", 1)
        self.assertEqual(report["status"], "handed_in")
        self.assertEqual(len(api.puts), 1)

    def test_changes_requested_stops(self):
        reviews = [{"user": {"login": "rev"}, "state": "CHANGES_REQUESTED",
                    "submitted_at": "2026-10-01T11:00:00Z"}]
        api = FakeApi({
            "/pulls/1": [json.loads(json.dumps(PR_OPEN))],
            "/branches/main/protection": [PROTECTION],
            f"/commits/{SHA}/check-runs?per_page=100": [CHECKS_GREEN],
            "/pulls/1/commits?per_page=100": [COMMITS],
            "/pulls/1/reviews?per_page=100": [reviews],
        })
        report = pr_watch.run_once(api, "o/r", 1)
        self.assertEqual(report["status"], "changes_requested")
        self.assertEqual(api.puts, [])

    def test_stale_changes_requested_ignored(self):
        reviews = [{"user": {"login": "rev"}, "state": "CHANGES_REQUESTED",
                    "submitted_at": "2026-10-01T09:00:00Z"}]
        api = FakeApi({
            "/pulls/1": [json.loads(json.dumps(PR_OPEN))],
            "/branches/main/protection": [PROTECTION],
            f"/commits/{SHA}/check-runs?per_page=100": [CHECKS_GREEN],
            "/pulls/1/commits?per_page=100": [COMMITS],
            "/pulls/1/reviews?per_page=100": [reviews],
        })
        report = pr_watch.run_once(api, "o/r", 1)
        self.assertEqual(report["status"], "handed_in")

    def test_merged_stops(self):
        pr = json.loads(json.dumps(PR_OPEN))
        pr["merged"] = True
        pr["merge_commit_sha"] = "deadbeef"
        api = FakeApi({"/pulls/1": [pr]})
        report = pr_watch.run_once(api, "o/r", 1)
        self.assertEqual(report["status"], "merged")
        self.assertEqual(api.puts, [])

    def test_merged_report_names_cleanup_targets(self):
        pr = json.loads(json.dumps(PR_OPEN))
        pr["merged"] = True
        pr["merge_commit_sha"] = "deadbeef"
        api = FakeApi({"/pulls/1": [pr]})
        with patch.object(pr_watch, "worktree_for_branch",
                          return_value="/tmp/wt") as wt:
            report = pr_watch.run_once(api, "o/r", 1)
        self.assertEqual(report["cleanup"],
                         {"branch": "feat/1-x", "worktree": "/tmp/wt"})
        wt.assert_called_once_with("feat/1-x")

    def test_merged_report_null_worktree(self):
        pr = json.loads(json.dumps(PR_OPEN))
        pr["merged"] = True
        api = FakeApi({"/pulls/1": [pr]})
        with patch.object(pr_watch, "worktree_for_branch",
                          return_value=None):
            report = pr_watch.run_once(api, "o/r", 1)
        self.assertEqual(report["cleanup"]["worktree"], None)

    def test_worktree_for_branch_parses_porcelain(self):
        porcelain = (
            "worktree /home/u/main-wt\n"
            "HEAD 1111\n"
            "branch refs/heads/main\n"
            "\n"
            "worktree /home/u/feat-wt\n"
            "HEAD 2222\n"
            "branch refs/heads/feat/1-x\n"
            "\n"
        )
        ok = subprocess.CompletedProcess([], 0, stdout=porcelain, stderr="")
        with patch.object(pr_watch.subprocess, "run", return_value=ok):
            self.assertEqual(pr_watch.worktree_for_branch("feat/1-x"),
                             "/home/u/feat-wt")
            self.assertIsNone(pr_watch.worktree_for_branch("fix/9-y"))

    def test_worktree_for_branch_git_failure_returns_none(self):
        with patch.object(pr_watch.subprocess, "run",
                          side_effect=FileNotFoundError("git")):
            self.assertIsNone(pr_watch.worktree_for_branch("feat/1-x"))
        failed = subprocess.CompletedProcess([], 1, stdout="", stderr="x")
        with patch.object(pr_watch.subprocess, "run", return_value=failed):
            self.assertIsNone(pr_watch.worktree_for_branch("feat/1-x"))

    def test_issue_number_from_branch(self):
        self.assertEqual(pr_watch.issue_number_from_branch("docs/239-x"), 239)
        self.assertEqual(pr_watch.issue_number_from_branch("feat/12-a-b"), 12)
        self.assertIsNone(pr_watch.issue_number_from_branch("junk"))
        self.assertIsNone(pr_watch.issue_number_from_branch(None))

    def _git_responder(self, dirty_worktree=False, push_fails=False):
        porcelain = ("worktree /home/u/main\nHEAD 1111\n"
                     "branch refs/heads/main\n\n"
                     "worktree /tmp/wt\nHEAD 2222\n"
                     "branch refs/heads/docs/239-x\n\n")

        def fake(args):
            if args[:1] == ["-C"] and "status" in args:
                out = " M x" if dirty_worktree else ""
                return subprocess.CompletedProcess(args, 0, stdout=out, stderr="")
            if args[:2] == ["worktree", "list"]:
                return subprocess.CompletedProcess(args, 0, stdout=porcelain,
                                                   stderr="")
            if args[:2] == ["worktree", "remove"]:
                return subprocess.CompletedProcess(args, 0, stdout="", stderr="")
            if args[:1] == ["branch"]:
                return subprocess.CompletedProcess(args, 0, stdout="", stderr="")
            if args[:1] == ["push"]:
                code = 1 if push_fails else 0
                return subprocess.CompletedProcess(args, code, stdout="",
                                                   stderr="push boom")
            return subprocess.CompletedProcess(args, 1, stdout="", stderr="?")
        return fake

    def _merged_report(self):
        return {"pr": 240, "status": "merged", "merge_commit_sha": "abc123def",
                "cleanup": {"branch": "docs/239-x", "worktree": "/tmp/wt"}}

    def test_cleanup_removes_and_comments(self):
        api = StubApi()
        with patch.object(pr_watch, "_git",
                          side_effect=self._git_responder()):
            result = pr_watch.post_merge_cleanup(api, self._merged_report())
        self.assertEqual(result["issue"], 239)
        self.assertEqual(api.comments[0][0], "/issues/239/comments")
        body = api.comments[0][1]["body"]
        self.assertIn("squash `abc123def`", body)
        self.assertIn("worktree `/tmp/wt`: removed", body)
        self.assertIn("local branch `docs/239-x`: deleted", body)
        self.assertIn("remote branch `docs/239-x`: deleted", body)
        self.assertIn("main @ /home/u/main, clean", body)

    def test_cleanup_keeps_dirty_worktree(self):
        api = StubApi()
        with patch.object(pr_watch, "_git",
                          side_effect=self._git_responder(dirty_worktree=True)):
            pr_watch.post_merge_cleanup(api, self._merged_report())
        body = api.comments[0][1]["body"]
        self.assertIn("kept (uncommitted changes)", body)

    def test_cleanup_tolerates_git_and_push_failures(self):
        api = StubApi()
        with patch.object(pr_watch, "_git",
                          side_effect=self._git_responder(push_fails=True)):
            pr_watch.post_merge_cleanup(api, self._merged_report())
        body = api.comments[0][1]["body"]
        self.assertIn("delete failed", body)
        self.assertIn("possibly already auto-deleted", body)

    def test_cleanup_no_issue_without_branch_parse(self):
        api = StubApi()
        report = self._merged_report()
        report["cleanup"]["branch"] = "no-issue-here"
        with patch.object(pr_watch, "_git",
                          side_effect=self._git_responder()):
            result = pr_watch.post_merge_cleanup(api, report)
        self.assertIsNone(result["issue"])
        self.assertEqual(api.comments, [])

    def test_main_loop_runs_cleanup_on_merged(self):
        report = self._merged_report()
        with patch.object(pr_watch, "run_once", return_value=report), \
             patch.object(pr_watch, "post_merge_cleanup",
                          return_value={"issue": 239}) as cleanup:
            code = pr_watch.main(["o/r", "240", "--cleanup", "--interval", "1"])
        self.assertEqual(code, 0)
        cleanup.assert_called_once()

    def test_main_once_ignores_cleanup(self):
        report = self._merged_report()
        with patch.object(pr_watch, "run_once", return_value=report), \
             patch.object(pr_watch, "post_merge_cleanup") as cleanup:
            code = pr_watch.main(["o/r", "240", "--once", "--cleanup"])
        self.assertEqual(code, 0)
        cleanup.assert_not_called()


if __name__ == "__main__":
    unittest.main()
