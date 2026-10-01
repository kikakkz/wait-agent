#!/usr/bin/env python3
"""Unit tests for pr_watch.py against a scripted fake API."""

import json
import os
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
os.environ["PR_WATCH_STATE_DIR"] = tempfile.mkdtemp(prefix="pr-watch-test-")

import pr_watch


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
PR_OPEN = {"merged": False, "state": "open", "head": {"sha": SHA},
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


if __name__ == "__main__":
    unittest.main()
