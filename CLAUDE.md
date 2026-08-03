# CLAUDE.md — Downpour

You are working on **Downpour**, a cross-platform (Linux + Windows) download manager engine
in Rust. This file is the entry point. It is deliberately short; the depth lives in `docs/`.

## Read this first, every session

1. [`docs/agent/HARNESS.md`](docs/agent/HARNESS.md) — how work is done here. **Non-optional.**
2. [`state/progress.json`](state/progress.json) — what stage we are in and what is next.
3. [`docs/agent/CONTEXT-MAP.md`](docs/agent/CONTEXT-MAP.md) — which document to open for the
   task you were given. Do not read all of `docs/`; read the two or three that apply.

If you are about to write engine code, also read
[`docs/agent/INVARIANTS.md`](docs/agent/INVARIANTS.md). It is short and every line is load-bearing.

## The five rules

1. **Correctness outranks speed.** A silently corrupted file is a catastrophic bug. Being
   20% slower than IDM is a normal bug. Never trade the first for the second.
2. **Stay inside the current stage.** The roadmap in `state/progress.json` defines an active
   stage. Work that belongs to a later stage does not get written early, even if it is easy.
   If you believe a stage boundary is wrong, say so and stop — do not cross it silently.
3. **Every behavioural claim needs a test that would fail without it.** "It should handle
   expired URLs" is not done until a corpus case reproduces an expired URL and the test
   goes red when the handling is removed.
4. **Update `state/progress.json` in the same change that alters project status.** A `Stop`
   hook enforces this. See [`.claude/skills/progress-update/SKILL.md`](.claude/skills/progress-update/SKILL.md).
5. **Irreversible technical choices become ADRs.** If you are picking between two libraries,
   two data models, or two protocols, write `docs/adr/NNNN-*.md` before the code, using
   `/adr-new`. Record the trade-off honestly, including what would make us reverse it.

## Project facts you should not re-derive

| Fact | Value |
| ---- | ----- |
| Language | Rust, stable channel, edition 2024, MSRV pinned in `rust-toolchain.toml` |
| Async runtime | Tokio (multi-threaded). No second runtime in the same process. |
| Metadata store | SQLite in WAL mode via `rusqlite` |
| Durability | Per-download append-only recovery journal + sparse preallocated target file |
| IPC | JSON-RPC 2.0 over Unix domain socket (Linux) / named pipe (Windows) |
| Process model | `downpourd` daemon owns all state; CLI, GUI, and native host are clients |
| Licence | Apache-2.0 for all first-party code |
| Binaries | `downpourd`, `dp` (CLI), `downpour` (GUI), `downpour-host` (native messaging) |

Version-pinned dependency baseline: [`docs/14-tech-radar-2026.md`](docs/14-tech-radar-2026.md).
Do not invent versions — check that file, and if it is stale, verify against crates.io and
update it.

## Commands

*(These land in Stage 1 when the workspace exists. Until then they are the target, not the
current state.)*

```bash
cargo fmt --all -- --check          # formatting gate
cargo clippy --all-targets --all-features -- -D warnings
cargo nextest run --workspace       # unit + integration
cargo test --doc                    # doctests
just corpus                         # replay the compatibility corpus
just sim                            # deterministic simulation suite
bash scripts/doctor.sh              # environment check
node scripts/check-progress.mjs     # validate state/progress.json
```

## What not to do

- Do not add a dependency without checking it against `docs/14-tech-radar-2026.md` and
  recording it if it is significant.
- Do not write a GUI before Stage 10. The UI is intentionally last so the toolkit choice
  stays reversible.
- Do not use `unwrap()` / `expect()` / `panic!` in daemon or engine code paths. See
  `.claude/rules/rust.md`.
- Do not implement DRM circumvention or a TLS MITM proxy. This is a hard boundary, not a
  priority call. See [`docs/10-security-privacy-legal.md`](docs/10-security-privacy-legal.md).
- Do not decompile, disassemble, or copy from IDM. Black-box study only.
- Do not mark work complete in `progress.json` that you have not actually verified running.

## Optional specialist toolkits

`plugins/` is an in-repo Claude Code plugin marketplace. Install it once:

```bash
claude plugin marketplace add ./plugins
```

Then install what the current stage needs — `downpour-protocol` for HTTP/RFC work,
`downpour-qa` for corpus and simulation work, `downpour-release` for packaging.
See [`plugins/README.md`](plugins/README.md).
