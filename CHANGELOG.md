# Changelog

Notable changes to Downpour. Written for humans, not generated from commit subjects.

Format: [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
Versioning: [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
Pre-1.0, the minor position carries breaking changes.

## [Unreleased]

### Added — Stage 2: Range, resume, validators, crash-safe storage

The stage that can be silently wrong, and the one everything after it depends on.

- **`downpour-storage`** — the crate that owns durability. A sparse preallocated `.dppart`, an
  append-only checksummed recovery journal, and a writer that commits in one fixed order:
  write, sync the data, append the journal record, sync the journal, and only then mark the
  range complete. Reversing any two of those is the corruption bug that has shipped in most
  open-source download managers at some point, so the ordering is pinned by a simulation that
  kills the process at every boundary and compares the recovered file byte for byte against an
  independent generator.
- **Resume that refuses to splice.** A resumed request carries `If-Range` with the validator
  recorded when the existing bytes were fetched. Anything but a `206` is a hard stop, never
  permission to continue: the alternative is a file that is half one version and half another,
  of exactly the expected size, which every integrity check that does not hash the content
  passes. A weak `ETag` is not usable for this and does not pretend to be.
- **Verification before naming.** A `.dppart` becomes the real filename only after its length,
  its gap-free coverage, and the server's RFC 9530 digest all agree. Each answers a question the
  others cannot — a file can be exactly the right size and still be a hole surrounded by data.
- **Recovery after an unclean shutdown.** The journal is the only authority; SQLite is a
  disposable cache that can be stale, ahead, or unreadable, and none of that may change what is
  marked complete. Nothing auto-resumes on start, because auto-starting ten transfers on a
  metered connection is how a download manager gets uninstalled.
- **A full disk pauses rather than fails**, and the part file is never shortened to make room.
  The bytes we would resume from live in that extent, and the user's other data is not ours to
  sacrifice either.
- **`tests/sim`** — deterministic crash and disk-full simulation. Nothing here is timed or
  raced: a failure names the boundary that broke rather than "sometimes".
- The compatibility corpus grew to 73 cases across `ranges`, `framing`, `validators`, `local`,
  `redirects`, `connections` and `session`, and found two real bugs that had been latent since
  S1 — a filename truncated to exactly the filesystem limit with no room for the `.dppart`
  suffix, and a journal that outlived its download and blocked the next attempt at the same URL.
- ADRs 0012–0018, each with a reversal trigger.

### Added — Stage 1: Single-stream downloader

- **`downpour-http`** — HTTP/1.1 and HTTP/2 behind a `TransferProtocol` trait, so the backend is
  swappable and the engine never sees a `reqwest` type.
- **A capability probe that believes only what it observes.** `Accept-Ranges` is advertisement;
  only a validated `206` with a consistent `Content-Range` proves range support.
- **`dp`**, the command-line client, and the `downpour-types` and `downpour-intervals` crates —
  the latter exhaustively property-tested, because an interval map that can overlap is an
  interval map that will.
- The corpus and its pathology server: declarative YAML cases against a frozen deterministic
  content generator, with a runner that fails closed on anything it does not understand.

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
