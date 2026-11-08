#!/usr/bin/env python3
"""pr_watch — own the PR review-fix loop end to end (see .agents/skills/pr-watch).

The tool observes and reports; it never diagnoses or fixes. It polls an open
PR's required checks and review decisions, prints an actionable JSON report,
and hands in (squash merge / merge-queue entry) exactly once per head SHA
when green and undecided.

Usage:
  pr_watch.py REPO PR_NUMBER [--once] [--interval SECONDS] [--max-rounds N]
             [--cleanup]

Exit codes: 0 merged; 2 actionable stop (failing checks, changes requested,
max rounds); 3 user interrupt. Auth: GITHUB_TOKEN env. Stdlib only.

On merged the JSON report carries post-merge cleanup targets: the head
branch and the path of its local git worktree, when one exists.

With --cleanup (loop mode; ignored with --once) the watcher performs the
post-merge cleanup itself after the merge: remove the branch's worktree
when clean, delete the branch locally and on the remote, check
main-worktree hygiene, and post the result to the issue named by the
branch (`<type>/<issue>-<slug>`).
"""

import argparse
import json
import os
import re
import signal
import subprocess
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

    def post(self, path, payload):
        req = urllib.request.Request(self.base + path, method="POST",
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


def worktree_for_branch(branch):
    try:
        out = subprocess.run(["git", "worktree", "list", "--porcelain"],
                             capture_output=True, text=True, timeout=15)
    except (OSError, subprocess.SubprocessError):
        return None
    if out.returncode != 0:
        return None
    path = None
    for line in out.stdout.splitlines():
        if line.startswith("worktree "):
            path = line[len("worktree "):]
        elif line == f"branch refs/heads/{branch}":
            return path
    return None


def issue_number_from_branch(branch):
    m = re.match(r"^[a-z]+/(\d+)-", branch or "")
    return int(m.group(1)) if m else None


def _git(args):
    env = dict(os.environ, GIT_TERMINAL_PROMPT="0")
    try:
        return subprocess.run(["git"] + args, capture_output=True, text=True,
                              timeout=30, env=env)
    except (OSError, subprocess.SubprocessError) as e:
        return subprocess.CompletedProcess(args, 1, "", str(e))


def main_worktree_status():
    out = _git(["worktree", "list", "--porcelain"])
    if out.returncode != 0 or not out.stdout.startswith("worktree "):
        return "unknown (not inside a git worktree)"
    lines = out.stdout.splitlines()
    first = lines[0][len("worktree "):]
    ref = next((l for l in lines if l.startswith("branch ")), None)
    name = ref[len("branch refs/heads/"):] if ref and \
        ref.startswith("branch refs/heads/") else "detached"
    dirty = _git(["-C", first, "status", "--porcelain"])
    state = "clean" if dirty.returncode == 0 and \
        not dirty.stdout.strip() else "dirty"
    return f"{name} @ {first}, {state}"


def post_merge_cleanup(api, report):
    cleanup = report.get("cleanup") or {}
    branch = cleanup.get("branch")
    worktree = cleanup.get("worktree")
    lines = []
    if worktree:
        dirty = _git(["-C", worktree, "status", "--porcelain"])
        if dirty.returncode == 0 and dirty.stdout.strip():
            lines.append(f"- worktree `{worktree}`: kept (uncommitted changes)")
        else:
            r = _git(["worktree", "remove", worktree])
            lines.append(f"- worktree `{worktree}`: removed" if r.returncode == 0
                         else f"- worktree `{worktree}`: remove failed"
                              f" ({r.stderr.strip()[:120]})")
    else:
        lines.append("- worktree: none found")
    if branch:
        r = _git(["branch", "-D", branch])
        lines.append(f"- local branch `{branch}`: deleted" if r.returncode == 0
                     else f"- local branch `{branch}`:"
                          f" {r.stderr.strip()[:120]}")
        r = _git(["push", "origin", "--delete", branch])
        tail = (r.stderr.strip() or r.stdout.strip())[:120]
        lines.append(f"- remote branch `{branch}`: deleted" if r.returncode == 0
                     else f"- remote branch `{branch}`: delete failed"
                          f" ({tail}; possibly already auto-deleted)")
    lines.append(f"- main worktree: {main_worktree_status()}")
    body = (f"PR #{report['pr']} merged (squash "
            f"`{str(report.get('merge_commit_sha'))[:12]}`).\n\n"
            f"Post-merge cleanup:\n" + "\n".join(lines))
    issue = issue_number_from_branch(branch)
    if issue:
        api.post(f"/issues/{issue}/comments", {"body": body})
    return {"issue": issue, "body": body}


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
        head_ref = pr["head"]["ref"]
        report["cleanup"] = {"branch": head_ref,
                             "worktree": worktree_for_branch(head_ref)}
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
    if states and all(s["status"] == "missing" for s in states.values()):
        report["status"] = "checks_pending"
        report["pending"] = list(states)
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
        if report["merged"]:
            report["merge_commit_sha"] = result.get("sha")
            head_ref = pr["head"]["ref"]
            report["cleanup"] = {"branch": head_ref,
                                 "worktree": worktree_for_branch(head_ref)}
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
    parser.add_argument("--cleanup", action="store_true",
                        help="loop mode: run post-merge cleanup on merge")
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
            if args.cleanup and not args.once:
                result = post_merge_cleanup(api, report)
                print(json.dumps({"status": "cleanup", "result": result},
                                 indent=2))
            return 0
        if status in ("failing_checks", "changes_requested",
                      "closed_unmerged", "hand_in_failed", "api_error"):
            return 2
        if args.once:
            return 0
        if status == "handed_in":
            if args.cleanup:
                if report.get("merged"):
                    result = post_merge_cleanup(api, report)
                    print(json.dumps({"status": "cleanup", "result": result},
                                     indent=2))
                    return 0
                time.sleep(args.interval)
                continue
            return 0
        time.sleep(args.interval)
    print(json.dumps({"status": "max_rounds_exceeded",
                      "rounds": args.max_rounds}))
    return 2


if __name__ == "__main__":
    sys.exit(main())
