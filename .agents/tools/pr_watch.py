#!/usr/bin/env python3
"""pr_watch — own the PR review-fix loop end to end (see .agents/skills/pr-watch).

The tool observes and reports; it never diagnoses or fixes. It polls an open
PR's required checks and review decisions, prints an actionable JSON report,
and hands in (squash merge / merge-queue entry) exactly once per head SHA
when green and undecided.

Usage:
  pr_watch.py REPO PR_NUMBER [--once] [--interval SECONDS] [--max-rounds N]

Exit codes: 0 merged; 2 actionable stop (failing checks, changes requested,
max rounds); 3 user interrupt. Auth: GITHUB_TOKEN env. Stdlib only.
"""

import argparse
import json
import os
import signal
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

STATE_DIR = Path(os.environ.get("PR_WATCH_STATE_DIR",
                                Path.home() / ".cache" / "pr-watch"))
INTERRUPTED = False


def _handle_sigint(signum, frame):
    global INTERRUPTED
    INTERRUPTED = True


class Api:
    def __init__(self, repo, token):
        self.base = f"https://api.github.com/repos/{repo}"
        self.headers = {
            "Authorization": f"Bearer {token}",
            "Accept": "application/vnd.github+json",
            "User-Agent": "pr-watch",
            "Content-Type": "application/json",
        }

    def get(self, path):
        req = urllib.request.Request(self.base + path, headers=self.headers)
        with urllib.request.urlopen(req) as r:
            return json.load(r)

    def put(self, path, payload):
        req = urllib.request.Request(self.base + path, method="PUT",
                                     data=json.dumps(payload).encode(),
                                     headers=self.headers)
        try:
            with urllib.request.urlopen(req) as r:
                return json.load(r), r.status
        except urllib.error.HTTPError as e:
            return {"error": e.read().decode()[:300]}, e.code


def state_file(repo, pr):
    safe = repo.replace("/", "-")
    return STATE_DIR / f"{safe}-{pr}.json"


def load_state(repo, pr):
    f = state_file(repo, pr)
    if f.is_file():
        return json.loads(f.read_text())
    return {"handed_in": []}


def save_state(repo, pr, state):
    STATE_DIR.mkdir(parents=True, exist_ok=True)
    state_file(repo, pr).write_text(json.dumps(state, indent=2))


def required_contexts(api, base_branch):
    try:
        protection = api.get(f"/branches/{base_branch}/protection")
    except urllib.error.HTTPError as e:
        if e.code == 404:
            return []
        raise
    checks = protection.get("required_status_checks") or {}
    return checks.get("contexts") or []


def check_states(api, sha, contexts):
    runs = api.get(f"/commits/{sha}/check-runs?per_page=100")
    by_name = {c["name"]: c for c in runs.get("check_runs", [])}
    out = {}
    for name in contexts:
        c = by_name.get(name)
        if c is None:
            out[name] = {"status": "missing", "conclusion": None,
                         "url": None}
        else:
            out[name] = {"status": c["status"], "conclusion": c.get("conclusion"),
                         "url": c.get("html_url")}
    return out


def latest_head_commit_date(api, pr_num):
    commits = api.get(f"/pulls/{pr_num}/commits?per_page=100")
    last = commits[-1]["commit"]["committer"]["date"]
    return last


def changes_requested(api, pr_num, head_date):
    reviews = api.get(f"/pulls/{pr_num}/reviews?per_page=100")
    latest = {}
    for r in reviews:
        user = r["user"]["login"]
        if r["state"] != "COMMENTED":
            latest[user] = (r["state"], r["submitted_at"])
    blockers = [u for u, (s, t) in latest.items()
                if s == "CHANGES_REQUESTED" and t > head_date]
    approvers = [u for u, (s, t) in latest.items() if s == "APPROVED"]
    return sorted(blockers), sorted(approvers)


def hand_in(api, pr_num, sha):
    return api.put(f"/pulls/{pr_num}/merge",
                   {"merge_method": "squash", "expected_head_sha": sha})


def failing_checks(states):
    return {n: s for n, s in states.items()
            if s["status"] in ("completed", "missing")
            and s["conclusion"] != "success"}


def run_once(api, repo, pr_num):
    pr = api.get(f"/pulls/{pr_num}")
    report = {"pr": pr_num, "head_sha": pr["head"]["sha"], "url": pr["html_url"]}
    if pr.get("merged"):
        report["status"] = "merged"
        report["merge_commit_sha"] = pr.get("merge_commit_sha")
        return report
    if pr.get("state") != "open":
        report["status"] = "closed_unmerged"
        return report

    sha = pr["head"]["sha"]
    contexts = required_contexts(api, pr["base"]["ref"])
    states = check_states(api, sha, contexts)
    report["checks"] = states
    pending = [n for n, s in states.items()
               if s["status"] not in ("completed", "missing")]
    if pending:
        report["status"] = "checks_pending"
        report["pending"] = pending
        return report
    bad = failing_checks(states)
    if bad:
        report["status"] = "failing_checks"
        report["failing"] = bad
        return report

    head_date = latest_head_commit_date(api, pr_num)
    blockers, approvers = changes_requested(api, pr_num, head_date)
    report["approvers"] = approvers
    if blockers:
        report["status"] = "changes_requested"
        report["blockers"] = blockers
        return report

    state = load_state(repo, pr_num)
    if sha in state["handed_in"]:
        report["status"] = "handed_in_waiting"
        return report

    result, code = hand_in(api, pr_num, sha)
    if code == 200:
        state["handed_in"].append(sha)
        save_state(repo, pr_num, state)
        report["status"] = "handed_in"
        report["merged"] = result.get("merged", False)
        return report
    report["status"] = "hand_in_failed"
    report["http_status"] = code
    report["error"] = result
    return report


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("repo", help="owner/repo")
    parser.add_argument("pr", type=int, help="pull request number")
    parser.add_argument("--once", action="store_true", help="single observation")
    parser.add_argument("--interval", type=int, default=30, help="poll seconds")
    parser.add_argument("--max-rounds", type=int, default=120)
    args = parser.parse_args(argv)

    token = os.environ.get("GITHUB_TOKEN")
    if not token:
        print("ERROR: GITHUB_TOKEN not set.", file=sys.stderr)
        return 2
    signal.signal(signal.SIGINT, _handle_sigint)
    api = Api(args.repo, token)

    for round_no in range(args.max_rounds):
        if INTERRUPTED:
            print(json.dumps({"status": "interrupted", "round": round_no}))
            return 3
        try:
            report = run_once(api, args.repo, args.pr)
        except urllib.error.HTTPError as e:
            print(json.dumps({"status": "api_error", "http_status": e.code,
                              "body": e.read().decode()[:300]}))
            return 2
        print(json.dumps(report, indent=2))
        status = report["status"]
        if status == "merged":
            return 0
        if status in ("failing_checks", "changes_requested",
                      "closed_unmerged", "hand_in_failed", "api_error"):
            return 2
        if args.once or status == "handed_in":
            return 0
        time.sleep(args.interval)
    print(json.dumps({"status": "max_rounds_exceeded",
                      "rounds": args.max_rounds}))
    return 2


if __name__ == "__main__":
    sys.exit(main())
