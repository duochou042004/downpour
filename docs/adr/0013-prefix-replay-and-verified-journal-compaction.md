# ADR-0013: Split fatal journal headers from recoverable tails and verify data before compaction

- **Status:** proposed
- **Date:** 2026-08-04
- **Stage:** S2
- **Deciders:** maintainer (proposed by Codex)

## Context

ADR-0004 makes each download journal authoritative for durable byte coverage. ADR-0012 fixes
the version-1 bytes but intentionally leaves replay and compaction policy to S2-T3. Those
policies decide whether a damaged file loses progress, accepts progress that was never durable,
or silently blesses corrupt part-file bytes, so they are persistent storage semantics rather
than implementation details.

I-9 requires a self-validating, prefix-consistent replay that never panics. It does not mean
that every damaged byte sequence is a usable journal. The 72-byte header binds a journal to a
transfer id, object length and validator. Treating a truncated or invalid header as an empty
journal would discard that identity evidence and could attach bytes to the wrong remote object.
After a valid header, however, a partial final frame and a damaged complete frame are
indistinguishable after a crash. The safe result is the exact sequential prefix before the
first bad frame.

The draft compaction rule in `docs/04-storage-and-recovery-spec.md` says to emit a checkpoint
and a merged interval set. It omits a critical integrity step. BLAKE3 digests for adjacent
blocks cannot be algebraically combined into the BLAKE3 digest of their concatenation. Reading
the merged interval from the part file and hashing it without checking the old block digests
would turn pre-existing storage corruption into newly trusted journal evidence. Compaction
also needs a defined crash boundary on Linux and Windows and must retain v1 identity updates,
truncation, and sealing semantics.

## Options

| Option | Pros | Cons |
| ------ | ---- | ---- |
| **Strict header; recover a sequential record prefix; verify every old block digest before emitting a compacted v1 snapshot** | Preserves identity evidence; bounds crash loss to a record suffix; detects part-file corruption instead of certifying it; needs no new format or filesystem dependency | Compaction reads every covered byte and may read very large merged ranges in pieces; replay and semantic validation are more code |
| Preserve every `BlockComplete` frame byte-for-byte during compaction | Never reads the part file; original hashes stay authoritative | Cannot merge records, so journal size and replay time remain linear in completed-block count; does not satisfy the reason for compaction |
| Add a v2 `CoverageSnapshot` record containing an interval-map encoding | Compact and explicit; can preserve block boundaries separately from the snapshot | Introduces a migration and another persistent encoding before v1 has shipped; still needs a trustworthy way to produce snapshot digests |
| Trust current part bytes and recompute merged hashes | Simple and fast to implement | Silently blesses hardware or filesystem corruption, defeating the block hashes and I-9 |
| Store compaction checkpoints only in SQLite | Avoids a second journal representation | Reverses ADR-0004's authority rule and makes database loss or staleness a recovery correctness problem |

## Decision

**A valid v1 header is mandatory; record damage recovers only the exact consecutive prefix,
and compaction verifies every surviving `BlockComplete` digest before atomically replacing the
journal with a semantically equivalent v1 snapshot.**

Replay has two failure classes:

- Fewer than 72 bytes, bad magic, bad header CRC, unsupported version, or unsupported flags is
  a fatal error. File recovery does not modify the journal.
- After a valid header, replay expects sequence zero and then exact increments. Clean EOF is
  reported separately. A truncated frame, bad record CRC, unknown kind, malformed payload, or
  sequence gap stops before that frame. The result contains only fully decoded records and the
  byte offset immediately after the last valid sequential frame. File recovery truncates to
  that offset and synchronises the truncation before returning.

The decoder continues to bound each allocation by the v1 `u16` payload length. A record error
is recoverable only because the header version is already known to be v1; incompatible writers
must bump the header version under ADR-0012 and are therefore rejected before record replay.

Compaction first replays the recoverable prefix and validates its effective state. A
`BlockComplete` must have non-zero length, checked end arithmetic, lie within the effective
length, and not overlap another surviving completed range. A `Truncate` may only reduce the
effective length; any completed record crossing or beyond the new end is discarded rather
than cropped. A `Sealed` record must be unique and last. Semantically invalid state fails
compaction without changing the old journal.

Before creating replacement evidence, compaction reads every surviving original completed
range from the part source and compares its BLAKE3 digest with the journal. Any short read or
digest mismatch fails before replacement. Adjacent verified ranges may then be merged; ranges
larger than the v1 `u32` length field are split. Their replacement digests are computed from
the same individually verified byte stream. The compacted journal contains:

1. the unchanged header;
2. all opaque `IdentityUpdate` records in their original order, because S2-T3 may not interpret
   or coalesce the S2-T6 schema;
3. the final effective `Truncate`, when one occurred;
4. one `Checkpoint` whose `covered_bytes` equals the surviving merged coverage;
5. the verified, merged `BlockComplete` records; and
6. the original `Sealed` record last, when present.

Sequences are reassigned contiguously from zero. The header's `block_size` remains the initial
write granularity, not a maximum for compacted records. Replay before and after compaction must
produce the same effective length, ordered identity-update bytes, completed coverage and
digests over that coverage, and sealed digest.

Replacement writes a sibling `D.dpj.new`, synchronises it, renames it over `D.dpj` with
`std::fs::rename`, then synchronises the parent directory on platforms where directory sync is
available. At every injected boundary, recovery may observe the old journal or the new one,
but never a partially built file under the authoritative name. A leftover `.new` is not
authoritative.

## Consequences

**Easier:** fatal identity damage cannot masquerade as a new transfer. Every recoverable replay
state is an exact record prefix. Compaction reduces replay work while retaining v1 bytes and
cannot certify part data that contradicts the authoritative journal. The replacement protocol
uses Rust's cross-platform rename primitive without adding another filesystem dependency.

**Harder:** compaction is I/O proportional to durable coverage and must handle reads in bounded
chunks. Opaque identity deltas remain until S2-T6, so a pathological number of them cannot yet
be coalesced. Directory durability differs across platforms and needs platform-specific tests.

**Accepted:** a crash between rename and parent-directory sync may recover either the old or
new name after a power loss, depending on the filesystem. Both files are complete,
self-validating and semantically equivalent; accepting either is safe.

## Reversal trigger

Design a v2 snapshot record rather than reinterpreting v1 if either the compacted journal
cannot stay below 4 MiB or replay cannot meet ADR-0004's target of 100 ms for a 100 GB download.
Also reconsider mandatory verify-and-rehash compaction if Stage 4 measurements show it consuming
more than 1% of end-to-end transfer time in at least three benchmark conditions. Any faster
replacement must still prove that compaction cannot bless part bytes that fail their original
recorded digest.
