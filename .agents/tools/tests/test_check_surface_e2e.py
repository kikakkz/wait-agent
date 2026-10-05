#!/usr/bin/env python3
"""Unit tests for check_surface_e2e.py."""

import difflib
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

import check_surface_e2e

CLI_BASE = '''pub enum Command {
    Serve,
}

pub fn parse(args: &[String]) -> Result<Command, String> {
    match args.first().map(String::as_str) {
        "--port" => Ok(Command::Serve),
        _ => Err("unknown".to_string()),
    }
}
'''

CLI_WITH_NEW_FLAG = '''pub enum Command {
    Serve,
    Deploy,
}

pub fn parse(args: &[String]) -> Result<Command, String> {
    match args.first().map(String::as_str) {
        "--port" => Ok(Command::Serve),
        "--deploy-ttl" => Ok(Command::Deploy),
        _ => Err("unknown".to_string()),
    }
}
'''

# Reconstructing an existing flag (to_cli_args idiom) is not a new surface.
CLI_RECONSTRUCT_FLAG = CLI_BASE + '''
pub fn to_cli_args() -> Vec<String> {
    vec!["--port".to_string()]
}
'''

# A negative test that feeds an unknown flag to the parser is not a new arm.
CLI_TEST_UNKNOWN_FLAG = CLI_BASE + '''
#[cfg(test)]
mod tests {
    #[test]
    fn rejects_legacy() {
        let argv = ["waitagent", "--server", "127.0.0.1:7474"];
        assert!(!argv.is_empty());
    }
}
'''

CONFIG_BASE = '''pub struct RelayTomlConfig {
    pub address: String,
}

fn parse_key(key: &str) -> bool {
    match key {
        "address" => true,
        _ => false,
    }
}
'''

CONFIG_WITH_NEW_KEY = '''pub struct RelayTomlConfig {
    pub address: String,
    pub relay_fingerprint: String,
}

fn parse_key(key: &str) -> bool {
    match key {
        "address" => true,
        "relay_fingerprint" => true,
        _ => false,
    }
}
'''

PROTO_BASE = '''syntax = "proto3";

message ClientHello {
    string node_id = 1;
}
'''

PROTO_WITH_NEW_MESSAGE = '''syntax = "proto3";

message ClientHello {
    string node_id = 1;
}

message ServerHello {
    string challenge = 1;
}
'''

EMPTY_EXEMPTIONS = "# none\n"


class RepoFixture:
    """A throwaway git repository the script can point --repo at."""

    def __init__(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.dir = str(Path(self.tmp.name) / "repo")
        Path(self.dir).mkdir()
        self._git("init", "-q")
        self._git("config", "user.name", "Surface Test")
        self._git("config", "user.email", "surface@test.invalid")
        self.exemptions = Path(self.tmp.name) / "exemptions.txt"
        self.exemptions.write_text(EMPTY_EXEMPTIONS, encoding="utf-8")

    def _git(self, *args):
        return subprocess.run(
            ["git", "-C", self.dir, *args],
            capture_output=True,
            text=True,
            check=True,
        )

    def write(self, path, text):
        target = Path(self.dir) / path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(text, encoding="utf-8")

    def commit(self, message):
        self._git("add", "-A")
        self._git("commit", "-q", "-m", message)
        return self._git("rev-parse", "HEAD").stdout.strip()

    def head(self):
        return self._git("rev-parse", "HEAD").stdout.strip()

    def run_check(self, extra=None):
        argv = [
            "--repo", self.dir,
            "--exemptions", str(self.exemptions),
        ] + (extra or [])
        return check_surface_e2e.main(argv)

    def cleanup(self):
        self.tmp.cleanup()

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.cleanup()


class DetectionTests(unittest.TestCase):
    """Pure-function tests for the heuristics."""

    def diff_of(self, before, after, path="src/cli.rs"):
        lines = difflib.unified_diff(
            before.splitlines(),
            after.splitlines(),
            fromfile=f"a/{path}",
            tofile=f"b/{path}",
            n=0,
            lineterm="",
        )
        return "\n".join(lines) + "\n"

    def surfaces_of(self, before, after, path):
        files = check_surface_e2e.added_lines_per_file(
            self.diff_of(before, after, path)
        )
        return check_surface_e2e.detect_surfaces(files)

    def test_new_cli_flag_arm_detected(self):
        hits = self.surfaces_of(CLI_BASE, CLI_WITH_NEW_FLAG, "src/cli.rs")
        self.assertEqual([h.surface_id for h in hits], ["--deploy-ttl"])
        self.assertEqual(hits[0].kind, "CLI flag")

    def test_reconstructed_flag_not_detected(self):
        hits = self.surfaces_of(CLI_BASE, CLI_RECONSTRUCT_FLAG, "src/cli.rs")
        self.assertEqual(hits, [])

    def test_testfile_flag_array_not_detected(self):
        hits = self.surfaces_of(CLI_BASE, CLI_TEST_UNKNOWN_FLAG, "src/cli.rs")
        self.assertEqual(hits, [])

    def test_universal_help_arm_not_detected(self):
        after = CLI_BASE + '''
pub fn parse_sub(args: &[String]) -> bool {
    match args.first().map(String::as_str) {
        "--help" | "-h" => true,
        _ => false,
    }
}
'''
        hits = self.surfaces_of(CLI_BASE, after, "src/cli/newcmd.rs")
        self.assertEqual(hits, [])

    def test_config_key_arm_detected(self):
        hits = self.surfaces_of(
            CONFIG_BASE, CONFIG_WITH_NEW_KEY, "src/infra/relay_toml_store.rs"
        )
        self.assertEqual([h.surface_id for h in hits], ["key:relay_fingerprint"])

    def test_config_key_in_unrelated_file_ignored(self):
        hits = self.surfaces_of(
            CONFIG_BASE, CONFIG_WITH_NEW_KEY, "src/infra/other.rs"
        )
        self.assertEqual(hits, [])

    def test_web_config_file_detected(self):
        hits = self.surfaces_of(
            CONFIG_BASE, CONFIG_WITH_NEW_KEY, "src/web/config.rs"
        )
        self.assertEqual([h.surface_id for h in hits], ["key:relay_fingerprint"])

    def test_proto_message_detected(self):
        hits = self.surfaces_of(
            PROTO_BASE, PROTO_WITH_NEW_MESSAGE,
            "proto/waitagent/remote/v1/node_session.proto",
        )
        self.assertEqual([h.surface_id for h in hits], ["proto:ServerHello"])

    def test_line_numbers_tracked(self):
        hits = self.surfaces_of(CLI_BASE, CLI_WITH_NEW_FLAG, "src/cli.rs")
        self.assertTrue(any(h.line == 9 for h in hits), hits)


class ExemptionParsingTests(unittest.TestCase):
    def test_comments_and_blanks_ignored(self):
        exemptions, errors = check_surface_e2e.parse_exemptions(
            "# header\n\n--foo  because reasons\nkey:bar  tracked in #999\n"
        )
        self.assertEqual(errors, [])
        self.assertEqual(
            exemptions, {"--foo": "because reasons", "key:bar": "tracked in #999"}
        )

    def test_id_without_reason_rejected(self):
        _, errors = check_surface_e2e.parse_exemptions("--foo\n")
        self.assertEqual(len(errors), 1)

    def test_reason_that_is_only_a_comment_rejected(self):
        _, errors = check_surface_e2e.parse_exemptions("--foo  # todo\n")
        self.assertEqual(len(errors), 1)


class CliTests(unittest.TestCase):
    def test_head_mode_flag_without_e2e_fails(self):
        with RepoFixture() as repo:
            repo.write("src/cli.rs", CLI_BASE)
            first = repo.commit("base")
            repo.write("src/cli.rs", CLI_WITH_NEW_FLAG)
            second = repo.commit("add flag")
            self.assertEqual(repo.head(), second)
            self.assertNotEqual(first, second)
            self.assertEqual(repo.run_check(), 1)

    def test_head_mode_flag_with_e2e_passes(self):
        with RepoFixture() as repo:
            repo.write("src/cli.rs", CLI_BASE)
            repo.commit("base")
            repo.write("src/cli.rs", CLI_WITH_NEW_FLAG)
            repo.write("scripts/e2e/e2e-deploy.sh", "#!/bin/sh\nexit 0\n")
            repo.commit("add flag plus e2e")
            self.assertEqual(repo.run_check(), 0)

    def test_exemption_passes_and_missing_reason_fails(self):
        with RepoFixture() as repo:
            repo.write("src/cli.rs", CLI_BASE)
            repo.commit("base")
            repo.write("src/cli.rs", CLI_WITH_NEW_FLAG)
            repo.commit("add flag")

            repo.exemptions.write_text(
                "--deploy-ttl  relay deploy TTL cannot be e2e-tested yet; "
                "issue #140 tracks the harness gap\n",
                encoding="utf-8",
            )
            self.assertEqual(repo.run_check(), 0)

            repo.exemptions.write_text("--deploy-ttl\n", encoding="utf-8")
            self.assertEqual(repo.run_check(), 1)

    def test_range_mode(self):
        with RepoFixture() as repo:
            repo.write("src/cli.rs", CLI_BASE)
            base = repo.commit("base")
            repo.write("README.md", "docs\n")
            repo.commit("docs")
            repo.write("src/cli.rs", CLI_WITH_NEW_FLAG)
            head = repo.commit("add flag")
            self.assertEqual(
                repo.run_check(["--range", f"{base}..{head}"]), 1
            )

    def test_range_mode_backward_slice_without_flag_passes(self):
        with RepoFixture() as repo:
            repo.write("src/cli.rs", CLI_BASE)
            base = repo.commit("base")
            repo.write("src/cli.rs", CLI_WITH_NEW_FLAG)
            flag = repo.commit("add flag")
            repo.write("README.md", "docs\n")
            head = repo.commit("docs")
            # The flag landed before the range; the range itself adds none.
            self.assertEqual(
                repo.run_check(["--range", f"{flag}..{head}"]), 0
            )
            self.assertNotEqual(base, head)

    def test_config_and_proto_hits_in_head_mode(self):
        with RepoFixture() as repo:
            repo.write("src/infra/relay_toml_store.rs", CONFIG_BASE)
            repo.write(
                "proto/waitagent/remote/v1/node_session.proto", PROTO_BASE
            )
            repo.commit("base")
            repo.write("src/infra/relay_toml_store.rs", CONFIG_WITH_NEW_KEY)
            repo.write(
                "proto/waitagent/remote/v1/node_session.proto",
                PROTO_WITH_NEW_MESSAGE,
            )
            repo.commit("add key and message")
            self.assertEqual(repo.run_check(), 1)

    def test_no_surface_passes(self):
        with RepoFixture() as repo:
            repo.write("src/cli.rs", CLI_BASE)
            repo.commit("base")
            repo.write("src/cli.rs", CLI_BASE + "\n// a comment\n")
            repo.commit("comment")
            self.assertEqual(repo.run_check(), 0)

    def test_root_commit_all_files_are_new_surfaces(self):
        with RepoFixture() as repo:
            repo.write("src/cli.rs", CLI_WITH_NEW_FLAG)
            repo.commit("initial")
            # No parent: the whole tree is new, so the flag is unaccepted.
            self.assertEqual(repo.run_check(), 1)

    def test_bad_range_is_operational_error(self):
        with RepoFixture() as repo:
            repo.write("src/cli.rs", CLI_BASE)
            repo.commit("base")
            self.assertEqual(
                repo.run_check(["--range", "deadbeef..deadbeef"]), 2
            )

    def test_missing_e2e_change_but_exemption_file_unrelated(self):
        with RepoFixture() as repo:
            repo.write("src/cli.rs", CLI_BASE)
            repo.commit("base")
            repo.write("src/cli.rs", CLI_WITH_NEW_FLAG)
            repo.commit("add flag")
            repo.exemptions.write_text(
                "key:other  unrelated exemption stays unused\n",
                encoding="utf-8",
            )
            self.assertEqual(repo.run_check(), 1)


if __name__ == "__main__":
    unittest.main()
