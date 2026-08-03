---
paths:
  - "crates/**/*.rs"
  - "src/**/*.rs"
  - "tests/**/*.rs"
  - "**/Cargo.toml"
---

# Rust rules

## Errors

- **No `unwrap()`, `expect()`, or `panic!` in `downpour-engine`, `downpour-storage`,
  `downpour-http`, `downpour-ipc`, or `downpour-daemon`.** A panic in the daemon takes down
  every active transfer. Tests, build scripts, and `main()`'s top-level error report may unwrap.
- `thiserror` for library error enums. `anyhow` only at binary boundaries.
- Every error variant carries enough context to act on: which download, which offset, which URL.
  `Error::Io(std::io::Error)` with no context is not actionable in a bug report.
- Never discard a fallible result. `let _ = write(...)` is a review failure. If ignoring is
  genuinely correct, write `if let Err(e) = ... { tracing::warn!(...) }` and say why.
- Errors that cross the IPC boundary map to a stable `kind` string (`08-ipc-and-ui-spec.md` §3).
  Clients switch on `kind`, never on `message`, so `kind` values are API.

## Concurrency

- One Tokio runtime per process. Never construct a second.
- Blocking work — `fsync`, SQLite, filesystem metadata — goes through `spawn_blocking` or a
  dedicated thread. Blocking the executor stalls every transfer in the process.
- Prefer message passing to shared mutable state. The segment allocator is the single owner of
  the interval map; workers request and report, they do not mutate.
- Every channel between a producer and a consumer is **bounded**. An unbounded channel between
  the network and the disk is an out-of-memory bug waiting for a fast link.
- Holding a lock across an `.await` needs a comment justifying it.

## `unsafe`

- Requires a `// SAFETY:` comment stating the invariant being upheld, not restating what the
  code does.
- Non-trivial blocks need an ADR.
- Never for performance without a benchmark showing it matters.

## Dependencies

- Check `docs/14-tech-radar-2026.md` first. If the crate is in **Hold**, do not add it.
- Anything in the transfer, storage, or IPC path is significant and needs an ADR.
- Run `cargo tree` before and after. A crate that saves fifty lines and adds forty transitive
  dependencies is a bad trade.
- Apache-2.0-compatible licences only (`cargo deny` enforces this).

## Documentation

- Every public item in a `lib.rs` gets a doc comment.
- Every module gets a `//!` header naming **the invariant it owns**. "This module handles
  segments" is useless; "This module owns I-2: segments never overlap" is the point.
- Doc examples compile. `cargo test --doc` is in CI.

## Testing

- Pure data structures get `proptest`, not example tests. See `docs/09-testing-strategy.md` §2.
- Engine reactions to server behaviour get a corpus case, not a mock.
- Anything involving timing, ordering, or crash safety gets a simulation scenario with a seed.
- A test that breaks on a refactor with no behaviour change was testing the implementation.
  Rewrite it.

## Style

- `rustfmt` defaults. No custom `rustfmt.toml` without an ADR.
- `clippy -D warnings`. A new `#[allow(...)]` needs a comment saying why.
- Explicit types on public APIs. `impl Trait` in argument position is fine internally.
- Numeric conversions are explicit and checked: `u64 -> usize` is a `try_into()` with a real
  error, not an `as`. On 32-bit targets an `as` cast silently truncates a file offset, which is
  a corruption bug that only appears on the platform nobody tested.
