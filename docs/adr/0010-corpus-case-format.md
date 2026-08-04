# ADR-0010: Declarative corpus cases over a frozen deterministic content generator

- **Status:** proposed
- **Date:** 2026-08-03
- **Stage:** S1
- **Deciders:** maintainer (proposed by claude-code)

## Context

ADR-0007 committed to the compatibility corpus as this project's primary asset: IDM's real
moat is twenty years of accumulated knowledge about how servers misbehave, and the counter is
to generate that pathology space deliberately in a lab. `docs/09-testing-strategy.md` §3 sets
the target at roughly 159 cases across ten categories, growing permanently under Gate D — every
bug found anywhere becomes a case in the same change that fixes it.

That is a body of test data we will be maintaining for the life of the project, and two parts
of it are effectively permanent once cases start accumulating:

1. **The content generator.** Corruption detection is not "the checksum differs", it is "byte
   4 194 305 should be `0x7A` and is `0x00`, so the range starting at 4 MiB was never written
   despite being marked complete". That requires the correct value of any byte to be computable
   from a seed without storing the file — a 1 GB case cannot ship a 1 GB fixture. Every recorded
   expectation and every corruption finding is stated in terms of this function, so changing it
   silently invalidates all of them.
2. **The case schema.** Whatever shape the first cases take is the shape 150 more will take.

Both need deciding before the pathology server is written (S1-T6), not after.

There is also a specific failure mode to design against. A declarative test runner that
silently ignores an assertion it does not understand is worse than no runner: every case
reports green, the corpus metric climbs, and nothing is actually being checked. Risk R-7 in
`state/progress.json` is exactly this — "the corpus is not maintained and the strategy quietly
fails".

## Options

### The case representation

| Option | Pros | Cons |
| ------ | ---- | ---- |
| **A. Declarative YAML cases + one generic runner** | Cases are data: countable, greppable, diffable, cross-referenced to RFC clauses. The runner can impose assertions *every* case must pass, so no case can opt out of the corruption check. A contributor can add a case without writing Rust. | Needs a schema and a runner. A genuinely novel pathology may need the server's vocabulary extended rather than just some code written. |
| B. One Rust `#[test]` per case with a builder API | Maximally expressive, nothing to maintain but code, refactors alongside the engine. | Nothing structurally forces the uniform assertions, so the one that matters most gets forgotten under deadline. "159 cases" stops being a countable number. Easiest format in which to accidentally assert the implementation rather than the behaviour. |
| C. Pre-recorded HTTP traces replayed back | Real server behaviour, no fabrication. | Cannot express a *reaction* to what the client does — mid-transfer ETag changes, per-IP caps, expiring tokens are the whole point. Traces also cannot generate 1 GB of verifiable content. |

Option B is not a straw man; it is what most projects do, and for a normal test suite it would
be right. It loses here specifically because the corpus's value comes from being an auditable
*inventory* of known server misbehaviour, and an inventory has to be data.

### The content generator

| Option | Pros | Cons |
| ------ | ---- | ---- |
| **A. One seekable BLAKE3 XOF stream per seed** | `blake3` is already an adopted dependency, and the extendable-output mode is part of the published BLAKE3 specification, so output is stable across platforms, architectures and compiler versions. The XOF is seekable in constant time, so the whole construction is "seek to the offset and read" — no block arithmetic at all. Uniform output means a hole of zeros is detectable with overwhelming probability, and the content is incompressible, so a `gzip-on-range` bug cannot hide in it. **Measured 0.88 GiB/s** sequential and 107 ns for an isolated single-byte read. | Reading one byte still costs a hash initialisation. Irrelevant: bulk work goes through the fill path. |
| B. BLAKE3 in counter mode over fixed blocks | Same stability argument. | Strictly worse on both axes that matter. **Measured 0.25 GiB/s at a 32-byte block** and 0.86 GiB/s at 1024, so it is never faster than A, and it reintroduces block-index arithmetic — an off-by-one there corrupts only *some* offsets, which is precisely the failure mode this generator exists to detect. |
| C. A hand-rolled PRNG (xorshift, PCG) | Faster still, trivial to implement. | Stability depends on our own code staying bug-compatible with itself forever, and statistical quality becomes our problem. A PRNG with a short period or a weak low bit could mask exactly the corruption we are hunting. |
| D. `rand`'s `StdRng` | Convenient. | **Explicitly not reproducible across versions** — `rand` reserves the right to change `StdRng`'s algorithm. That single property disqualifies it. |
| E. A stored fixture file | Simplest of all. | A 1 GB binary in git, per size we want to test. |

Measurements are from `tests/corpus`, release profile, one core, generating 1 GiB in 1 MiB
buffers. They were taken *before* this ADR was accepted and before any case existed, which is
the only point at which the choice is free.

## Decision

**Corpus cases are declarative YAML over a frozen, named deterministic content generator, run
by a single generic runner that fails closed.**

### The generator, stated exactly

```text
stream(seed)     = BLAKE3 extendable output of seed.to_le_bytes()   // 8 bytes in
byte(seed, off)  = stream(seed)[off]                                 // XOF seek, O(1)
```

Two lines, and no arithmetic of our own. That is the point: the generator is the corpus's
oracle, so a bug *in the oracle* is the one bug that cannot be caught by the corpus. The
construction with the fewest moving parts is the correct one even before its speed advantage is
counted. BLAKE3's XOF is defined over a 2^64-byte output space, so every `u64` offset is valid.

This definition is **frozen**. It is named `blake3-ctr-v1` and every case states the generator
it uses. A future generator gets a *new name* and coexists; this one is never redefined,
because a redefinition would silently rewrite the meaning of every expectation and every
corruption finding already recorded.

### The case schema

```yaml
id: gzip-on-range                  # unique, kebab-case, stable — it is cited in commits
category: framing                  # one of the ten in docs/09-testing-strategy.md §3.2
description: >
  What the server does wrong and what the engine must do about it.
references: [RFC 9110 §8.4, INVARIANTS.md#i-5]   # required: every case is traceable
slow: false                        # true = excluded from the default profile

server:
  protocol: http/1.1               # or http/2
  content:
    size: 1MB
    generator: blake3-ctr-v1
    seed: 42
  ranges: supported                # supported | absent | lies | ignore
  headers: {}                      # literal headers to add or override
  behaviour:                       # ordered, triggered mutations
    - at: { bytes_served: 40% }
      then: { set_etag: '"v2"' }

expect:
  final_state: failed              # completed | failed | awaiting_refresh
  error_kind: unexpected_content_encoding
  file_renamed: false
```

Four contract rules, which are the whole point of choosing data over code:

1. **Every case is checked for corruption, and no case can opt out.** After the run the runner
   compares the produced file byte for byte against the generator. `silent_corruption: false`
   is not a key a case sets; it is unconditional. A non-zero finding fails the run *and* is a
   release blocker via `metrics.silent_corruption_findings`.
2. **Every case is checked for I-4.** `file_renamed` is asserted on every case. A failure case
   that asserts `file_renamed: false` is how "a partial file never gets the final name" is
   tested everywhere rather than once.
3. **The runner fails closed.** An unknown key, an unknown enum value, a missing `references`,
   or an `expect` block the runner does not understand is a hard error, never a skip. A
   deliberately-wrong fixture case lives under `tests/corpus/self-test/` and the runner is
   asserted to *fail* on it, so "the assertions are actually evaluated" is itself a test.
4. **`error_kind` values are the stable IPC `kind` strings** from `08-ipc-and-ui-spec.md` §3,
   not prose. Cases therefore pin the error taxonomy clients switch on.

There is no per-case schema version. Because unknown keys are a hard error, adding one is a
change that fails loudly on every case at once, and the cases live in this repository rather
than out in the world — so migration is a single mechanical commit, not a compatibility
problem. A version field would imply support for old shapes we have no reason to keep.

### The escape hatch

When a pathology genuinely cannot be expressed declaratively, a case may name a Rust-implemented
behaviour: `server: { plugin: cdn-edge-disagrees }`. This keeps the YAML vocabulary from growing
towards a programming language, which is the standard way a test DSL becomes the thing everyone
avoids. Reaching for the escape hatch is expected occasionally and is a smell in bulk — see the
reversal trigger.

## Consequences

**Easier:** the corpus becomes an auditable inventory — `ls tests/corpus/cases/ranges/ | wc -l`
is a real number, and `metrics.corpus_cases_total` can be trusted. Gate D becomes cheap: fixing
a bug means adding one YAML file, so the rule survives contact with deadlines. A 1 GB case costs
nothing in the repository. Because the corruption comparison is imposed by the runner rather
than written per case, the check that matters most cannot be the one that gets left out.

**Harder:** two artefacts must be maintained instead of one — the schema and the server's
vocabulary. Expressing a novel pathology is a two-step job: teach the server, then write the
case. Writing the runner's error handling to fail closed on malformed input is more work than
letting `serde` ignore unknown fields, and that work is the reason to do it.

**Accepted:** a generator that is slower than it could be, in exchange for output stability we
do not have to maintain ourselves. A YAML dialect specific to this project. And a real risk
that the declarative vocabulary proves too narrow, which the reversal trigger below is written
to catch early rather than at case 120.

## Reversal trigger

Three specific signals, in order of likelihood:

1. **The escape hatch stops being an exception.** If more than 20% of cases in any category
   need `server.plugin` by the end of S3, the declarative layer is not earning its keep for
   that category. Response: keep YAML for the case *inventory* and the uniform assertions, and
   move the server behaviour entirely into Rust — the countability is the part worth keeping,
   not the YAML.
2. **The generator becomes the bottleneck.** 0.88 GiB/s on one core is comfortably above what
   S1–S3 need and above a 1 Gbps benchmark condition, but it is not above a 10 Gbps one. If
   content generation costs more than 20% of corpus wall-clock time when the S4 benchmark work
   starts, define `blake3-ctr-v2` — parallel XOF fills across a thread pool would be the first
   thing to try, since the seek makes the stream trivially partitionable — add it alongside, and
   migrate cases deliberately. **Never redefine `v1`.**
3. **A corruption finding turns out to be the generator's fault.** If any reported corruption
   is ever traced to the generator rather than the engine, that is a correctness emergency for
   the whole corpus strategy: every past green result is suspect. Response is to freeze the
   corpus, add a self-test that pins `byte(seed, offset)` for a fixed vector table, and only
   then resume. That self-test should exist from the start — S1-T6's proof requires it.
