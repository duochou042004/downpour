# Downpour task runner. Works identically for humans, Claude Code, and Codex.
#   just            list recipes
#   just gate       run every gate that applies right now
#   just brief      print the session brief (stage, open tasks, unmet criteria)

set shell := ["bash", "-uc"]
export PATH := env_var('HOME') + "/.cargo/bin:" + env_var('PATH')

default:
    @just --list

# Everything a change must pass before it is proposed as done.
gate: progress sync-check shell-lint hook-test rust-gate
    @echo ""
    @echo "  all applicable gates passed"

# Validate state/progress.json against its schema and the project rules.
progress:
    @node scripts/check-progress.mjs

# Confirm the Codex plugin copy matches the canonical .claude/ assets.
sync-check:
    @scripts/sync-agent-assets.sh --check

# Regenerate the Codex plugin copy from .claude/.
sync:
    @scripts/sync-agent-assets.sh

# Environment check.
doctor:
    @bash scripts/doctor.sh

# The ORIENT step: what stage are we in and what is open.
brief:
    @bash .claude/hooks/session-brief.sh | jq -r '.hookSpecificOutput.additionalContext'

# Prove the RECORD-step guard still fires when it should, and stays quiet when it should not.
# A guard that cries wolf gets dismissed, which is the same outcome as no guard at all.
hook-test:
    @bash scripts/test-progress-guard.sh

shell-lint:
    @if command -v shellcheck >/dev/null 2>&1; then \
        shellcheck -S warning scripts/*.sh .claude/hooks/*.sh .githooks/* 2>/dev/null && echo "  shellcheck ok"; \
     else echo "  shellcheck not installed, skipped"; fi

# Rust gates. No-ops until the workspace exists in Stage 1.
rust-gate:
    @if [ -f Cargo.toml ]; then \
        cargo fmt --all -- --check && \
        cargo clippy --all-targets --all-features -- -D warnings && \
        cargo nextest run --workspace && \
        cargo test --doc; \
     else echo "  no Cargo.toml yet (Stage 1), rust gates skipped"; fi

# Replay the compatibility corpus. Stage 2 onward.
corpus:
    @if [ -d tests/corpus ]; then cargo nextest run -p downpour-corpus; \
     else echo "  tests/corpus does not exist yet (Stage 2)"; fi

# The slow corpus cases: the 1 GB transfers that prove S1-C1. Release, and excluded from
# `just gate` so the every-push budget stays under five minutes (docs/09 section 6).
corpus-slow:
    @if [ -d tests/corpus ]; then \
        cargo nextest run -p downpour-corpus --release --run-ignored all; \
     else echo "  tests/corpus does not exist yet"; fi

# Deterministic simulation suite. Stage 2 onward.
sim:
    @if [ -d tests/sim ]; then cargo nextest run -p downpour-sim --release; \
     else echo "  tests/sim does not exist yet (Stage 2)"; fi

# Install the repo git hooks. Run once after cloning.
init-hooks:
    @git config core.hooksPath .githooks && echo "  core.hooksPath = .githooks"

# Register the plugin marketplaces for both agents.
init-agents:
    @claude plugin marketplace add ./plugins 2>/dev/null || echo "  (claude CLI not present, skipped)"
    @codex plugin marketplace add . 2>/dev/null || echo "  (codex CLI not present, skipped)"

# Full first-time setup after a clone.
setup: init-hooks doctor
    @echo ""
    @echo "  Next: just brief"
