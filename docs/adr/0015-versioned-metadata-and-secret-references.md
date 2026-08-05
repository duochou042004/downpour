# ADR-0015: Use strict SQLite metadata with versioned CBOR evidence and secret references

- **Status:** accepted
- **Date:** 2026-08-04
- **Stage:** S2
- **Deciders:** maintainer (proposed by Codex)

## Context

ADR-0004 fixed the storage authority split: the append-only journal is authoritative for
durable transfer progress, while SQLite is the queryable store for downloads, identities,
history, and lagging checkpoints. S2-T6 is the first implementation of the SQLite side and
the first semantic interpretation of ADR-0012's opaque `IdentityUpdate` CBOR payload. The
remaining format choices therefore become persistent now and must be decided before code.

Four release-blocking invariants constrain the design:

- I-6: `RangeSupport::Proven` may only be constructed by validating an observed ranged
  response. Persisting `RangeProof` through a derived `Deserialize` implementation would
  create a second, unchecked constructor.
- I-8: the redirect chain and identity that actually worked must survive restart.
- I-11: an older binary must refuse a newer SQLite schema, checkpoint, range observation, or
  identity payload rather than guessing at it.
- I-14: cookies, authorization values, proxy credentials, request bodies containing secrets,
  and signed-URL query parameters must never reach SQLite or the journal in plaintext.

The draft schema in `docs/04-storage-and-recovery-spec.md` is not sufficient to implement
those rules literally. It has no place for the raw response that proved range support, calls
the checkpoint BLOB "compact" without defining bytes or a version, stores redirect chains as
unversioned JSON, and has plaintext URL columns even though a URL query may itself be a
secret. It also uses textual UUIDs while the journal already carries the same identity as 16
opaque bytes. Freezing those ambiguities as schema v1 would make later correction a migration.

The dependency choices are in the storage path and are part of this decision. They were
re-verified on 2026-08-04 against crates.io and their published sources:

- `rusqlite` 0.40.1 is MIT, directly exposes the SQLite transaction and pragma surface we
  need, and keeps the project free of an ORM. Its `bundled-windows` feature supplies SQLite
  on native Windows while Linux continues to use the system library. The crate declares no
  MSRV, so the pinned Rust 1.97.1 build and native Windows CI are required evidence.
- `ciborium` 0.2.2 and its `ciborium-io`/`ciborium-ll` companions are Apache-2.0, declare Rust
  1.58, and contain no production `unsafe` blocks in their published Rust sources. It gives
  us a maintained CBOR implementation without making `RangeProof` serializable.
- `serde` 1.0.229 is already the workspace serialization baseline. It is used only on private
  persisted DTOs; domain proof types remain non-deserializable.

No UUID dependency is needed in this layer. A download id is stored as the journal's existing
16 bytes, and a small checked type validates the RFC variant and version-7 bits. UUID
generation belongs at the daemon boundary, not in a synchronous storage codec.

## Options

| Option | Pros | Cons |
| ------ | ---- | ---- |
| **Normalized SQLite rows plus exact versioned CBOR arrays for evidence, checkpoints, and full identity snapshots; URLs are public components plus keyring references** | Queryable queue/history fields; one bounded binary vocabulary for the three opaque structures; raw range evidence is revalidated; exact golden bytes; journal snapshots can rebuild SQLite; plaintext secrets are structurally absent | More conversion and validation code; two representations must stay semantically aligned; schema changes require explicit migrations |
| Derive `Serialize`/`Deserialize` for `RemoteObject` and store JSON or CBOR blobs | Very little handwritten mapping; easy round trips | Necessarily makes `RangeProof` deserializable or adds an unchecked surrogate; serializer defaults become the format; difficult to query; easy to persist signed queries or request secrets by accident |
| Put every metadata field in normalized SQLite columns, including range observations and every identity delta | Maximum SQL visibility; no nested codec for checkpoints | Large sparse schema with coupled migrations; still does not solve ADR-0012's required CBOR identity payload; interval arrays are an unnatural row shape |
| Store one versioned CBOR blob per download and use SQLite only as a key/value table | One schema column and one codec; rebuild snapshots map directly to journal records | Reverses SQLite's query role from ADR-0004; queue ordering, state filtering, and history require decoding every row; indexes cannot protect common queries |
| Hand-encode new binary formats instead of using CBOR | Exact allocation and canonical-byte control; no serialization dependency | We would own another untrusted-input parser for variable strings, optional values, arrays, and schema evolution; substantially more audit surface than the fixed journal records justified |

Full identity snapshots and per-field identity deltas were also compared. Deltas are smaller,
but their meaning depends on every preceding update and makes a lost middle record harder to
reason about. A full snapshot is idempotent, lets the last valid snapshot rebuild SQLite by
itself, and remains bounded by ADR-0012's 65,535-byte record payload.

## Decision

**SQLite schema v1 is strict and normalized; opaque metadata uses canonical, versioned CBOR
arrays; journal identity records carry full snapshots; and persistent URLs/request context
contain only public components and typed keyring references.**

### SQLite opening and version policy

`downpour-storage` uses `rusqlite` 0.40.1 with default features disabled and
`bundled-windows` enabled. Every opened connection applies and verifies:

- `journal_mode = WAL` for file-backed databases;
- `synchronous = NORMAL`;
- `foreign_keys = ON`;
- `busy_timeout = 5000`; and
- `trusted_schema = OFF`.

Schema creation is one transaction. A database with `user_version = 0` becomes v1 only when
both `sqlite_schema` and its physical page set are empty, and only after every table and index
exists. Freelist remnants or another application's header make the file nonempty. A nonempty
unversioned database is refused without mutation. Version 1 is opened only after its exact
tables, constraints, and indexes validate. Any `user_version > 1` is a
`NewerSchemaVersion` error; no downgrade or best-effort read occurs. Future migrations are
explicit transactions from one named version to the next.

Download ids are 16-byte BLOB primary/foreign keys with `CHECK(length(id) = 16)`. The public
type validates UUID version 7 and the RFC variant before a row is written or trusted after a
read. Binary order preserves UUIDv7's time ordering without textual parsing.

Target and part paths are BLOBs containing a canonical CBOR pair governed by the enclosing
SQLite-schema or identity-snapshot version rather than SQLite text. Portable UTF-8 paths carry
tag `0`; non-UTF-8 Unix paths carry their exact bytes under tag `1`, and Windows paths that
cannot be represented as Unicode carry exact little-endian UTF-16 code units under tag `2`. A
native-only encoding is refused on the other platform, never decoded lossily. SQLite stores a
bounded lowercase dotted `error_kind` but no arbitrary server-originated error detail, because
that text can contain a signed URL or credential.

The draft `identities.redirect_chain` JSON column becomes a child table keyed by
`(download_id, hop)`. URL history likewise has an explicit entry ordinal rather than using a
millisecond timestamp as identity, so simultaneous observations retain order. The identity
row gains protocol, range state, and a nullable versioned range-observation BLOB. URL-bearing
rows store a `public_url` with no username, password, query, or fragment, plus an optional
`secret_ref`. Request context is an optional keyring reference, never JSON containing headers
or cookies. The storage API accepts these values as `PublicUrl` and `SecretRef` types rather
than raw secret-bearing strings.

### CBOR contract

CBOR is used for three structures:

1. a raw `RangeObservation` persisted with identity metadata;
2. the checkpoint's ordered set of complete intervals; and
3. a complete `DownloadIdentity` snapshot in each journal `IdentityUpdate` record.

Every top-level value is a definite-length array whose first element is unsigned format
version `1`. Every nested collection is also definite-length. CBOR semantic tags, maps,
floats, indefinite items, and serializer-dependent enum names are absent. Integers use their
shortest CBOR form, and optional values use `null`. Private tuple DTOs provide the only Serde
implementations. (`tag` below means an ordinary unsigned discriminator inside an array, not a
CBOR semantic tag.)
Decoded v1 data is re-encoded and must be byte-identical to the input, so alternative integer
widths, indefinite arrays, trailing values, and other non-canonical encodings are rejected.

The version prefix is inspected before a version-specific DTO is decoded. A newer version is
refused without interpreting its payload. Each collection and string has a limit checked
before allocation. Journal identity bytes retain ADR-0012's 65,535-byte hard maximum.
Checkpoint blobs are capped at 64 MiB and one million complete intervals; if a valid map
cannot fit, SQLite omits the cache and recovery uses the authoritative journal instead.

Complete checkpoint intervals must be non-empty, ordered, disjoint, within total length, and
their checked sum must equal both the encoded and SQL `covered_bytes` values. In-progress
grants are never persisted as checkpoint coverage. The encoded total must also equal the
owning download's immutable total before a cache row is written or trusted.

A raw range observation carries the requested `ByteRangeSpec`, status, `Content-Range`,
`Content-Encoding`, and body length. Loading it calls the existing
`RangeProof::from_observed_response` constructor. The persisted DTO may derive
`Deserialize`; `RangeProof` and `RangeSupport` may not.

An identity update is a full snapshot rather than a patch. It includes the download id,
lossless encoded paths and stable download metadata needed for rebuild, public URL references, redirect chain,
URL history, validator, server digest, protocol, probe time, request-context reference, and
raw range evidence. The last valid snapshot wins. Block coverage remains exclusively in
`BlockComplete` records and checkpoints, preserving ADR-0004's authority split.

### Secret boundary

`PublicUrl` refuses userinfo, a query, or a fragment. If the working URL contains any of
those, the caller must first put the full value in the OS keyring and persist only a redacted
public URL plus its `SecretRef`. `SecretRef` is an opaque, length-bounded identifier; no API
accepts a cookie, authorization header, proxy credential, request body, or full signed URL as
metadata. Keyring access and URL reconstruction remain S7/S8 work, but S2 makes plaintext
secret persistence impossible through this storage surface.

## Consequences

**Easier:** I-6 remains compiler-enforced; every persistent structure has a direct newer-
version refusal test; checkpoint corruption cannot create trusted overlap; database rebuild
has an idempotent identity snapshot; common queue and history queries remain ordinary SQL;
and a raw SQLite/journal scan cannot reveal signed query strings or request credentials.

**Harder:** callers must manage keyring values separately from public URL metadata; schema
validation and DTO conversion are explicit; full identity snapshots use more journal space
than deltas; the Linux build depends on a sufficiently recent system SQLite while Windows
compiles the bundled source.

**Accepted:** SQLite checkpoints may be omitted when their cache representation exceeds the
bound, increasing replay work without weakening correctness. We also accept some duplicated
identity data in exchange for prefix-independent journal recovery.

## Reversal trigger

Replace `ciborium` behind byte-identical golden fixtures if it becomes unmaintained, gains a
security issue relevant to bounded decoding, or fails the pinned MSRV. Do not reinterpret
existing version-1 bytes; use a new metadata version for an incompatible representation.

Reconsider full snapshots if real identity updates exceed ADR-0012's payload limit in more
than 0.1% of corpus or opt-in telemetry samples. The replacement must be a new journal format
that preserves prefix recovery and newer-version refusal; silently truncating identity state
is not an option.

Reconsider the SQLite linking policy if native Linux or Windows packaging proves unable to
provide a security-patched SQLite consistently. Changing linkage must not change schema bytes
or the journal authority rule.
