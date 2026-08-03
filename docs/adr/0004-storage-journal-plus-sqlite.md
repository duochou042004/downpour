# ADR-0004: Append-only journal for progress, SQLite for metadata

- **Status:** accepted
- **Date:** 2026-08-02
- **Stage:** S0

## Context

Invariant I-1 requires that no byte range is marked complete before its data is durable. That
means a durability record on a hot path: potentially thousands of small completions per
download.

The natural first instinct is "put the block map in SQLite". Measured against I-1, that is a
transaction per block, or holding uncommitted progress in memory — which is exactly the thing
that produces the classic corruption bug.

## Options

| Option | Pros | Cons |
| ------ | ---- | ---- |
| **Journal for progress + SQLite for metadata** | O(1) appends; trivially correct crash semantics; a torn tail is a prefix, not corruption; SQLite stays available for the queries that need it | Two stores; a rebuild path is needed when they disagree |
| SQLite only | One store | Transaction per block, or batching in memory (violates I-1); a corrupt DB loses everything |
| Journal only | Simplest durability | Queue queries, filtering, and history become manual scans |
| An embedded KV store (`redb`, `sled`) | Transactional, fast | Another dependency in the most critical path, for a data shape that an append-only file models better |

## Decision

**Both, with a clear authority rule.**

- **Per-download append-only journal** (`.dpj`): block completions, checkpoints, identity
  updates. CRC32C per record; replay stops at the first bad record. This is the durability
  mechanism.
- **SQLite (WAL)**: queue, identities, URL history, compatibility profiles, and periodic
  interval-map checkpoints. This is the query mechanism.
- **The journal wins.** If they disagree, SQLite is rebuilt from `journals/`. `dp repair` does
  exactly that.

SQLite runs with `synchronous = NORMAL` precisely because it is *not* our durability
mechanism; the journal is.

## Consequences

**Easier:** durability is cheap and provable; a corrupt or deleted database is an
inconvenience, not data loss; journal replay is small enough to property-test exhaustively
(I-9); flush batching is a tunable performance knob that cannot compromise correctness.

**Harder:** two formats to version (I-11); journal compaction is needed for large files;
a rebuild path must exist and must be tested, not assumed.

**Accepted:** slightly more code in exchange for a storage layer whose failure modes we can
enumerate.

## Reversal trigger

If journal replay time becomes significant for very large files even after compaction
(target: under 100 ms for a 100 GB download), reconsider the record format — not the split.
The two-store split is the durable decision; the record encoding is a detail.
