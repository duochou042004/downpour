# Changelog

Notable changes to Downpour. Written for humans, not generated from commit subjects.

Format: [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
Versioning: [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
Pre-1.0, the minor position carries breaking changes.

## [Unreleased]

### Added — Stage 0: Foundations

- The specification set in `docs/`: architecture, transfer engine, storage and recovery,
  protocol matrix, browser integration, media, IPC and UI, testing strategy, security, and
  packaging.
- ADRs 0001–0009 covering every irreversible choice made so far, each with a reversal trigger.
- `docs/agent/INVARIANTS.md` — the fourteen properties whose violation blocks a release.
- The agent harness: `CLAUDE.md`, `AGENTS.md`, and `docs/agent/` — a working loop with gates
  that both Claude Code and Codex follow.
- `state/progress.json` with a JSON Schema and a zero-dependency validator that enforces the
  project's own rules, not just the shape.
- A dual-agent plugin marketplace: Claude Code via `plugins/.claude-plugin/marketplace.json`,
  Codex via `.agents/plugins/marketplace.json`, sharing one set of skills.
- `.githooks/pre-commit` — agent-agnostic enforcement at the commit boundary.
- `justfile`, `scripts/doctor.sh`, `scripts/check-progress.mjs`, `scripts/sync-agent-assets.sh`.
- GitLab CI pipeline.

### Notes

No product code yet. Stage 1 is the first single-stream downloader. The roadmap and its exit
gates are in `docs/12-roadmap-stages.md`; the authoritative status is `state/progress.json`.
