# ADR-0012: Use a manually encoded, versioned binary recovery journal

- **Status:** accepted
- **Date:** 2026-08-04
- **Stage:** S2
- **Deciders:** maintainer (proposed by Codex)

## Context

ADR-0004 fixed the durable architecture: per-download append-only journals are authoritative
for block progress, while SQLite is the queryable metadata store. It deliberately left the
journal's byte encoding reversible. S2-T2 is the point where that encoding becomes persistent,
so it must be fixed before any codec code or fixture exists.

The format has to uphold two release-blocking invariants:

- I-9: every record is self-validating, and a damaged tail cannot invalidate an earlier prefix;
- I-11: an older binary refuses state written in a newer format rather than guessing.

It must also be identical on Linux and Windows, deterministic enough for byte-for-byte fixture
tests, bounded before allocation, and decodable without panicking on untrusted disk bytes.

The draft in `docs/04-storage-and-recovery-spec.md` contained a decision-blocking ambiguity: it
called the header 64 bytes, but the listed fields occupy 72 bytes
(`4 + 2 + 2 + 16 + 8 + 4 + 32 + 4`). Encoding the diagram literally and encoding its label
would create two incompatible v1 formats. This ADR resolves that before either is implemented.

The checksum implementation is part of the storage path and therefore also needs an explicit
choice. Research was re-verified on 2026-08-04 against crates.io and the published source:
`crc` 3.4.0 supports Rust 1.83, is MIT/Apache-2.0, depends only on `crc-catalog`, and both crates
forbid unsafe code. `crc32c` 0.6.8 offers hardware acceleration but declares no MSRV and uses
architecture-specific unsafe paths. The journal is synchronised to storage in batches, so CRC
throughput is not presently evidence that justifies unsafe in the most correctness-sensitive
path.

## Options

### Record encoding

| Option | Pros | Cons |
| ------ | ---- | ---- |
| **Manual fixed fields in little-endian order, with an explicitly framed variable payload** | Exact bytes are reviewable; no padding or platform ABI; bounded lengths are checked before allocation; corrupt-field behaviour is explicit; byte fixtures pin compatibility | We own roughly a few hundred lines of codec and must maintain migrations deliberately |
| `serde` plus `bincode` or `postcard` | Compact; less hand-written field code; familiar derived round trips | The persistent layout depends on serializer configuration and enum representation; schema evolution and limits are less visible; journal framing and CRC coverage still need custom code |
| CBOR for the whole journal | Self-describing; unknown map fields can be skipped; already specified for identity deltas | Variable and non-canonical encodings complicate golden bytes; generic nesting and allocation enlarge the corrupt-input surface; substantially more parser than fixed numeric records need |
| SQLite rows for every record | No new binary format | Reverses ADR-0004 and either forces a transaction per block or recreates unsafe in-memory batching; the database becomes the single corruption domain |

The serializer options are reasonable for metadata whose schema changes often. They are a poor
fit for five small record kinds whose important property is an auditable commit boundary.

### CRC32C implementation

| Option | Pros | Cons |
| ------ | ---- | ---- |
| **`crc` 3.4.0 with `CRC_32_ISCSI`** | Established implementation of the Castagnoli polynomial; pure safe Rust; one tiny transitive dependency; declared MSRV below ours; easy exit because one private function wraps it | Generic software implementation leaves hardware acceleration unused |
| `crc32c` 0.6.8 | Purpose-built API; runtime hardware acceleration on x86_64 and AArch64; software fallback | Multiple unsafe paths in a release-blocking storage dependency; no declared MSRV; last crate release was 2024-06 |
| A local CRC32C implementation | No dependency; format logic entirely in-tree | We would own polynomial tables and optimisation code whose only failure mode is accepting corrupt state; saving one small dependency is not worth that audit burden |

## Decision

**Journal v1 is a manually encoded little-endian binary format with a 72-byte header and
CRC-32/ISCSI checksums supplied by `crc` 3.4.0.**

The byte contract is:

### File header — exactly 72 bytes

| Offset | Size | Field |
| -----: | ---: | ----- |
| 0 | 4 | ASCII magic `DPJ1` |
| 4 | 2 | format version, little-endian `u16`; v1 is `1` |
| 6 | 2 | flags, little-endian `u16`; every bit is zero in v1 |
| 8 | 16 | opaque download id bytes |
| 24 | 8 | total length, little-endian `u64` |
| 32 | 4 | block size, little-endian `u32` |
| 36 | 32 | opaque validator hash bytes |
| 68 | 4 | little-endian CRC32C of bytes `[0, 68)` |

The format layer does not interpret the download id or validator hash. UUIDv7 generation and
validator canonicalisation belong to S2-T6; keeping them opaque prevents T2 from pulling UUID,
CBOR, URL, or validator semantics into the crash-recovery codec.

### Record — 15 bytes overhead plus payload

| Offset | Size | Field |
| -----: | ---: | ----- |
| 0 | 8 | sequence, little-endian `u64` |
| 8 | 1 | kind (`0x01` through `0x05`) |
| 9 | 2 | payload length, little-endian `u16` |
| 11 | N | payload, at most 65,535 bytes |
| 11 + N | 4 | little-endian CRC32C of bytes `[0, 11 + N)` |

CRC coverage includes sequence, kind, and length as well as payload. Checksumming only payload
would let a flipped length or kind redirect the parser while still presenting a valid checksum.

Fixed payloads are also little-endian and length-exact:

| Kind | Payload bytes |
| ---- | ------------- |
| `0x01 BlockComplete` | `offset u64`, `len u32`, BLAKE3 `[u8; 32]` — 44 bytes |
| `0x02 Checkpoint` | `covered_bytes u64`, `wall_clock u64` — 16 bytes |
| `0x03 IdentityUpdate` | opaque CBOR bytes — 0 through 65,535 bytes; semantic validation is S2-T6 |
| `0x04 Truncate` | `new_length u64` — 8 bytes |
| `0x05 Sealed` | final BLAKE3 `[u8; 32]` — 32 bytes |

Decoders reject a bad magic, bad header checksum, any non-zero v1 flag, unknown record kind,
wrong fixed-payload length, checksum mismatch, truncated field, and any format version other
than the versions they explicitly support. A new record kind or incompatible semantic requires
a header version bump; an older binary then refuses the whole journal under I-11 instead of
mistaking a new record for a torn tail. Replay's policy for turning a record error into a valid
prefix belongs to S2-T3, not this codec.

Golden fixtures pin the exact header and every record kind in addition to typed round trips.
The known CRC32C vector `123456789 -> 0xe3069283` independently proves that the selected
algorithm is Castagnoli rather than the more common IEEE CRC-32.

## Consequences

**Easier:** the on-disk contract is independent of Rust layout, operating system, and serializer
defaults. Every allocation is bounded by a wire `u16`. I-11 is a direct version comparison.
Mutation tests can flip version, flags, kind, length, payload, and checksum independently.

**Harder:** compatible evolution requires a deliberate v2 and migration path. The codec is
more code than a derive macro. Identity CBOR is carried opaquely until S2-T6 validates it.

**Accepted:** software CRC may be slower than hardware CRC32C. Durability syncs dominate this
path today, and correctness plus a smaller unsafe surface outrank an unmeasured speedup.

## Reversal trigger

Replace the private checksum backend, without changing journal bytes, if a Stage 4 benchmark
shows CRC computation consuming more than 5% of writer CPU at the fastest supported transfer
condition. A replacement must keep the golden vectors and all journal fixtures byte-identical.

Introduce a new journal version rather than redefining v1 if either (a) real migrations require
old binaries to skip unknown records safely, or (b) ADR-0004's existing replay target cannot be
met after compaction (under 100 ms for a 100 GB download). Existing v1 readers must continue to
refuse the newer header; existing v1 bytes are never reinterpreted.
