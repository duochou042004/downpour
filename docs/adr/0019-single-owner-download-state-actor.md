# ADR-0019: Serialize allocation and durable writes through one download-state actor

- **Status:** proposed
- **Date:** 2026-08-09
- **Stage:** S3
- **Deciders:** maintainer (proposed by Codex)

## Context

S3 introduces several workers for one file, which makes invariant I-2 live for the first time.
`docs/02-architecture.md` says the download supervisor owns the interval map, workers never mutate
it, and one writer task owns the file handle. `docs/04-storage-and-recovery-spec.md` adds that
workers send positional writes over a bounded channel and that the writer rejects an offset outside
the sender's current grant.

The S2 implementation does not yet have those separate tasks. `StorageSink` owns both an
`IntervalMap` and a synchronous `DurableWriter`; each accepted HTTP chunk moves that state through
`spawn_blocking` and back. `DurableWriter::stage` checks ownership against the map, and
`DurableWriter::flush` changes the same map to `Complete` only after data sync, journal append, and
journal sync. That coupling is useful: the write barrier and I-1's commit point consult and update
the same authority. It also means simply moving the writer to a dedicated task while leaving the
map in an async supervisor would create two owners or require both tasks to mutate one object.

A second mutable grant registry is not an acceptable shortcut. Two individually canonical maps can
disagree about ownership, and canonical structure would not reveal that two workers believe they
own the same bytes. S2-T16 found exactly this class of weak proof: an accepted double grant kept the
partition canonical. S3 must make exclusive ownership structural, not eventual.

Splitting and worker death add another ordering constraint. A block may be written but still staged
when a grant is shortened or abandoned. Mutating ownership first makes the later durability flush
reject its own staged block; allowing the old block through after reassignment risks overlap.
Therefore every ownership-revoking command needs a fence that commits already accepted staged work
before the canonical map changes. Writes queued after that command must be checked against the new
map and refused if they carry stale authority.

## Options

| Option | Pros | Cons |
| ------ | ---- | ---- |
| **One per-download state actor owns `SegmentAllocator`, `DurableWriter`, part file, and journal; async code uses typed bounded command handles** | Exactly one canonical ownership map; allocation, write validation, durability completion, split fences, and abandon fences are totally ordered; existing `DurableWriter` I-1 proofs remain on the production path; a stale grant is checked against current state immediately before I/O | One dedicated blocking thread per active download; allocation commands can wait behind filesystem I/O; actor shutdown and error propagation need explicit protocol tests |
| Async supervisor owns the map; writer owns a shadow grant registry | Supervisor decisions stay cheap and writer I/O remains isolated | Two ownership models can diverge; every grant, split, completion, and abandon needs a transactional two-phase protocol; a crash between phases has no honest authority; doubles the I-2 proof surface |
| Share one `Arc<Mutex<IntervalMap>>` between async supervisor and writer | One data object and little API redesign | Blocking filesystem work while holding the lock stalls scheduling; dropping the lock around I/O reintroduces a check/use race; poison and cancellation semantics leak across the engine; ownership becomes runtime convention rather than type structure |
| Remove interval mutation from `DurableWriter`; return durable blocks for the async supervisor to complete | Clean separation between storage and scheduling; no dedicated allocator thread | The writer still needs a current, revocable authority to reject stale writes, so this merely moves the second-map problem into grant tokens; the supervisor can die after journal commit but before updating its map and must replay before continuing; safe split fencing remains a separate protocol |
| Keep moving all state through `spawn_blocking` per chunk | Already proven correct for one worker; no thread lifecycle | Serialises the caller by physically taking the sink, so multiple workers cannot share it; about sixteen thousand handoffs per 1 GiB at 64 KiB chunks; does not implement the bounded writer service required by S3 and ADR-0016's reversal trigger |

## Decision

**Use one per-download state actor as the sole owner of the `SegmentAllocator`, `DurableWriter`,
part file, and journal, with typed async handles over a bounded FIFO command channel.**

The async transfer supervisor still owns worker task lifecycle, retries, protocol decisions, and
cancellation. It owns the state actor's control handle, so the actor is the supervisor's private
state authority rather than a second scheduler. Only that control handle can request allocation,
split, completion fences, abandonment, snapshots, or shutdown. A worker receives a narrower
grant-writer handle that can only append bytes from the cursor fixed by its allocator-issued grant;
it has no API for selecting an absolute offset or mutating allocation state.

The actor runs on one dedicated blocking thread for each active download. Tokio's bounded MPSC is
the wake-up and back-pressure mechanism; the receiver uses its blocking side, so no filesystem call
runs on the async executor and no second async runtime exists. Channel capacity is a configuration
with a documented finite default. Write commands also have a fixed maximum payload, so “bounded”
means bounded bytes rather than merely a bounded count of arbitrarily large vectors.

Every command is processed in FIFO order:

1. A write carries an opaque allocator-issued grant and bytes only. The grant-writer computes the
   next offset. Immediately before I/O, the actor checks that exact range against its current map.
2. `DurableWriter::stage` performs the positional write and may cross the existing byte-bound flush.
   A receipt distinguishes accepted-but-staged bytes from `DurableBlock`s that crossed I-1's commit
   point; no caller may read “command returned” as “Complete”.
3. Before an allocation that would split active work, or before abandonment, the actor flushes all
   staged blocks. Only after that succeeds may it mutate ownership. This is the split/abandon fence.
4. A write ordered before the fence is either committed under the old owner or fails the fence. A
   stale write ordered after it is rejected by the canonical map before touching the part file.
5. Shutdown flushes staged work, returns a final snapshot, and joins the thread. Dropping a worker
   handle alone never changes ownership; the async supervisor explicitly reports death so reclaim is
   observable and testable.

No persistent format changes. The recovery journal remains authoritative, and restart still rebuilds
only `Complete` and `Pending`; no in-memory actor or `InProgress` state is persisted.

## Consequences

**Easier:** I-1 and I-2 share one serialization point and one interval map. The existing durable
writer remains the only path from bytes to journal evidence. Fault injection can wrap the same
`DurableData` and `DurableJournal` traits in production-shaped tests, and every allocator mutation
has a precise position relative to staged writes and syncs. Worker-facing types make “workers do not
choose offsets” an API property.

**Harder:** Each active download consumes a blocking thread and a bounded queue. Allocation can wait
behind an ongoing filesystem call, so S4 must measure decision latency rather than assume it is
negligible. The actor needs explicit startup, shutdown, channel-closure, writer-poisoning, disk-full,
and reply-cancellation behavior. A worker death notification cannot be inferred from a dropped
sender, because multiple handles exist and implicit reclamation would be timing-dependent.

**Accepted:** split and abandon commands flush staged bytes before changing ownership. This may sync
earlier than the normal two-second/eight-MiB batch boundary. The cost is ordinary lost batching; the
alternative is an interval-map transition that cannot truthfully classify already written bytes.
Correctness wins, and S4 can optimise only with an equivalent fence proof.

**Accepted:** the actor is the physical owner of the map even though the async supervisor is the
logical owner of the transfer. The separation is private to `downpour-engine`: workers and protocol
backends see grants and receipts, never the actor or map.

## Reversal trigger

Replace the per-download thread with a shared blocking actor pool if measurements with at least 64
simultaneously active downloads show thread memory or scheduling overhead is a material resource
limit, provided the replacement preserves one serialized authority per download and the same
mutation-verified split/abandon fences.

Decouple allocation from the blocking actor if S4's fixed `slow-worker` and interleaving simulations
show command latency behind filesystem I/O exceeds `DECISION_INTERVAL` or causes at least a 5%
repeatable tail-time regression. Any replacement must first demonstrate, under a mutation that
removes or reorders its revocation fence, that stale writes and premature completion make the named
I-1/I-2 tests fail. A faster design with a second mutable ownership map is not a valid reversal.
