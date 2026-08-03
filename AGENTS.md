# AGENTS.md — Downpour

Guidance for any coding agent working in this repository (Codex CLI, Cursor, Jules, Copilot,
Aider, Gemini CLI, Zed, and others that read `AGENTS.md`).

Claude Code reads [`CLAUDE.md`](CLAUDE.md), which carries the same rules plus Claude-specific
tooling. **If the two files ever disagree, this one and `CLAUDE.md` must be reconciled in the
same commit — they are intentionally kept in sync.**

---

## Project overview

**Downpour** is a cross-platform (Linux + Windows) download manager built in Rust, aiming at
feature and reliability parity with Internet Download Manager while using the current
protocol generation (HTTP/2, HTTP/3/QUIC) rather than IDM's HTTP/1.1-era architecture.

The project is currently **specification-stage**. There is no Rust workspace yet. The
authoritative status of every stage is in [`state/progress.json`](state/progress.json).

Architecture: a `downpourd` daemon owns all transfer state; a CLI, a desktop GUI, and a
browser native-messaging host are all clients over a versioned JSON-RPC 2.0 IPC contract.
Full detail in [`docs/02-architecture.md`](docs/02-architecture.md).

## Read before you write

Read these three, in order, at the start of every session:

1. [`docs/agent/HARNESS.md`](docs/agent/HARNESS.md) — the operating manual: how a unit of work
   starts, what gates it, how it finishes.
2. [`state/progress.json`](state/progress.json) — the current stage and its open tasks.
3. [`docs/agent/CONTEXT-MAP.md`](docs/agent/CONTEXT-MAP.md) — which spec document covers the
   task you were handed. Read only those; `docs/` is large by design.

If the task touches transfer, segmentation, or storage, also read
[`docs/agent/INVARIANTS.md`](docs/agent/INVARIANTS.md).

Codex-specific notes, including sandbox and approval settings that this repo expects:
[`docs/agent/CODEX-NOTES.md`](docs/agent/CODEX-NOTES.md).

## The five rules

1. **Correctness outranks speed.** Silent file corruption is a catastrophic bug; being 20%
   slower than IDM is a normal one. Never trade the first for the second.
2. **Stay inside the current stage.** `state/progress.json` names an active stage. Do not
   implement later-stage features early, even when trivial. Flag the boundary and stop.
3. **Every behavioural claim needs a test that fails without it.** No exceptions for
   "obviously correct" code — the engine's failure modes are precisely the ones that look
   obviously correct.
4. **Update `state/progress.json` in the same change that changes project status.**
   `node scripts/check-progress.mjs` validates it; CI rejects a stale or malformed file.
5. **Irreversible choices become ADRs** in `docs/adr/`, written *before* the code, using the
   template at [`docs/adr/README.md`](docs/adr/README.md).

## Build and test commands

The Rust workspace arrives in Stage 1. Until then these are the target contract:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo nextest run --workspace
cargo test --doc
just corpus          # replay the compatibility corpus
just sim             # deterministic simulation suite
bash scripts/doctor.sh
node scripts/check-progress.mjs
```

A change is not complete until `fmt`, `clippy -D warnings`, `nextest`, and
`check-progress.mjs` all pass locally.

## Code style

- Rust stable, edition 2024. MSRV is pinned in `rust-toolchain.toml`; do not raise it casually.
- **No `unwrap()`, `expect()`, or `panic!` in `downpourd` or engine crates.** Errors are values.
  `thiserror` for library errors, `anyhow` only at binary boundaries. Tests may unwrap.
- No `unsafe` without a `// SAFETY:` comment stating the invariant being upheld, and an ADR
  if the block is non-trivial.
- Public items in `crates/*/src/lib.rs` need doc comments. Modules need a `//!` header saying
  what invariant the module owns.
- Fallible I/O never silently ignores a result. `let _ = write(...)` is a review failure.
- Prefer explicit types on public APIs; `impl Trait` in argument position is fine internally.
- Naming: `snake_case` items, `SCREAMING_SNAKE` consts, crate names `downpour-*`.
- Formatting is `rustfmt` default. Do not hand-format; do not add a custom `rustfmt.toml`
  without an ADR.

TypeScript (browser extension, Stage 7+): strict mode on, no `any`, `biome` for lint+format.

## Testing instructions

Downpour's correctness strategy has three layers. A feature is not done until it is covered
at the layer that applies. Detail: [`docs/09-testing-strategy.md`](docs/09-testing-strategy.md).

1. **Property tests** (`proptest`) for the segment interval map and the recovery journal.
   These data structures have algebraic invariants; assert them, do not sample examples.
2. **Compatibility corpus** (`tests/corpus/`) — declarative YAML cases describing a server
   pathology (wrong `Content-Range`, `200` in response to a range request, ETag change
   mid-transfer, expiring token, connection cap, chunked with no `Content-Length`, …) plus
   the expected engine behaviour. **Every bug found in the wild is added here permanently.**
3. **Deterministic simulation** — the engine runs against a simulated network and clock so
   that a failing seed reproduces exactly. Crash injection at every write boundary belongs here.

Never write a test that depends on a live third-party server. The corpus server is local.

## Security considerations

Hard boundaries, not preferences:

- No DRM circumvention (Widevine, PlayReady, FairPlay, or any successor).
- No TLS man-in-the-middle proxy, ever, not even opt-in.
- No bypassing authentication, rate limits, or access controls. Downpour reuses the session
  the user's own browser already holds.
- Credentials and cookies live in the OS keyring, never in SQLite or logs.
- The IPC socket is user-scoped with restrictive permissions and an authentication token.
  It is not an HTTP server on localhost.
- Native messaging input is untrusted. Validate every field against the schema before use.
- IDM is studied black-box only: public docs and observed behaviour against our own test
  servers. No decompilation or disassembly.

Full document: [`docs/10-security-privacy-legal.md`](docs/10-security-privacy-legal.md).

## Commit and PR guidelines

- Conventional Commits: `feat(engine): …`, `fix(storage): …`, `docs(adr): …`,
  `test(corpus): …`, `chore(ci): …`.
- One logical change per commit. A commit that touches the engine and the packaging scripts
  is two commits.
- The PR body states: which stage, which exit criterion it advances, what test proves it, and
  what `state/progress.json` diff accompanies it.
- A PR that changes behaviour without changing tests will be rejected.

## Scope discipline

This project fails by sprawl, not by difficulty. When you find something worth doing that is
outside the current task:

- Add it to the `backlog` array in `state/progress.json` with a one-line rationale.
- Do not implement it now.
- Do not silently widen the change you were asked for.
