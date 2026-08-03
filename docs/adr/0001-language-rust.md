# ADR-0001: Rust as the implementation language

- **Status:** accepted
- **Date:** 2026-08-02
- **Stage:** S0

## Context

Downpour targets Linux and Windows, must run as a long-lived background daemon, must handle
many concurrent network transfers writing into one file at different offsets, and must be
correct under crash and concurrency conditions that are hard to test.

The origin discussion considered C#/.NET + Avalonia and Rust + a native toolkit. The initial
lean was C# on the grounds of development speed; it changed to Rust once the target became
both platforms and IDM-class quality.

## Options

| Option | Pros | Cons |
| ------ | ---- | ---- |
| **Rust** | No GC pauses in a long-lived daemon; data-race freedom at compile time — which matters enormously for a concurrent segment allocator; small static binaries; first-class QUIC/HTTP-3 stacks; excellent property-testing and fuzzing ecosystem | Slower to write; smaller GUI ecosystem; the h3 stack is still experimental |
| **C# / .NET 10 + Avalonia** | Fast to develop; mature GUI; `HttpClient` and `RandomAccess` cover the basics | Runtime dependency; GC pauses in a daemon holding many buffers; larger footprint; weaker story for QUIC-level control |
| **Kotlin / Compose Multiplatform** | Proven by AB Download Manager | JVM footprint is the thing users complain about in that product |
| **Go** | Simple concurrency, good stdlib, proven by gopeed | GC; less control over allocation in the hot write path; weaker property-testing ecosystem |
| **C++** | Maximum control | The concurrency and memory bugs we most need to avoid are exactly the ones C++ makes easiest |

## Decision

**Rust**, stable channel, edition 2024, for the engine, daemon, CLI, native messaging host,
and eventually the GUI.

Reasoning, in order of weight:

1. **The hardest bug class here is concurrent mutation of shared byte-range state.** Two
   workers writing the same offset (I-2) is the defect that produces silent corruption.
   Rust's ownership model makes the safe design the natural one rather than the disciplined one.
2. **The daemon is long-lived.** A GC pause during a coordinated fsync/journal sequence is a
   latency spike in exactly the wrong place, and the footprint of a JVM or .NET runtime sitting
   idle all day is precisely the complaint users have about the existing alternatives.
3. **Property testing and fuzzing are load-bearing here.** `proptest` and `cargo-fuzz` are how
   we prove the interval map and the journal, and Rust's ecosystem for both is strong.
4. **QUIC/HTTP-3 access.** `quinn` is a genuinely production-quality QUIC implementation. That
   matters for a stage-6 differentiator.
5. **One language for daemon, CLI, host, and GUI.** The browser extension is TypeScript
   regardless; adding a third language would not be free.

Explicitly *not* a reason: "Rust is faster than Kotlin." A download manager is bound by the
network, the server, and the disk. Rust buys **control and safety**, not raw throughput.
Claiming otherwise in the README would be dishonest and would set the wrong optimisation target.

## Consequences

**Easier:** fearless refactoring of the scheduler; small self-contained binaries with no
runtime to install; static analysis that catches the bug class we fear most; cross-compilation.

**Harder:** GUI development (ADR-0002); HTTP/3 (the Rust h3 stack is experimental, ADR-0005);
raw development speed, especially early.

**Accepted:** we are trading initial velocity for the ability to trust the engine. Given that
the scorecard makes correctness 25% of the total and silent corruption an automatic release
block, that is the right trade.

## Reversal trigger

We would reconsider if, by the end of Stage 4, the compatibility corpus shows the pure-Rust
HTTP stack failing a class of real-world servers that a mature C stack handles — and the gap
cannot be closed behind the `TransferProtocol` trait with a `curl` backend (see ADR-0005).
That would be evidence that the ecosystem is not ready, not that the language is wrong.
