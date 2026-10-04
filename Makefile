SHELL := /bin/bash

.PHONY: ci-gate check-branch test-tools check-trailers lint-sh ai-check e2e-relay

# The CI-first rule: every change lands together with its checks.
# Rust checks live in the pre-commit hook and .github/workflows/ci.yaml.
ci-gate: check-branch test-tools check-trailers lint-sh

# Branch names are cheapest to fix before push: a rename after a PR exists
# forces close-and-reopen (GitHub cannot retarget a PR).
check-branch:
	python3 .agents/tools/check_branch_name.py

test-tools:
	python3 -m unittest discover -s .agents/tools/tests -p 'test_*.py' -v

check-trailers:
	bash .agents/tools/check-trailers.sh

lint-sh:
	@if command -v shellcheck >/dev/null 2>&1; then \
		shellcheck .agents/tools/*.sh; \
		echo "shellcheck: OK"; \
	else \
		echo "shellcheck: not installed, skipped"; \
	fi

ai-check:
	.github/scripts/check-agents-integrity.sh

# Relay docker e2e (issue #37): isolated two-nodes-plus-relay topology.
# Set WA_E2E_BINARY to a prebuilt binary to skip the in-docker release build.
e2e-relay:
	@scripts/e2e/relay/e2e-relay.sh
