# Engine Invariants

**Every line here is load-bearing. A violation of any of these is a release blocker, not a bug.**

These are the properties that separate a download manager you can trust from one you cannot.
Most open-source download managers have shipped a violation of at least one of I-1 through
I-5 at some point in their history — including ones that had been stable for years. They are
listed first because they are the ones that get broken.

Each invariant is stated as an assertion that must hold, followed by the mechanism that
enforces it and the test layer that proves it. Run `/invariant-check` before proposing any
engine, storage, or IPC change as done.

---

## Data integrity

### I-1 — A byte range is never marked complete before its data is durable

**Assertion:** for any interval `[a, b)` recorded as `Complete` in the block map, the bytes
`[a, b)` in the target file have been written *and* the write has been forced to stable
storage, in that order, before the block map record is committed.

**Why:** the classic corruption. Process dies between "wrote to page cache" and "fsync";
the block map says complete; resume skips the range; the file has a hole full of zeros or
stale data and the size and the checksum both still look right.

**Mechanism:** write → `fdatasync`/`FlushFileBuffers` → append journal record → (later)
checkpoint into SQLite. The journal append is the commit point. Never the other way round.

**Proof:** deterministic simulation with crash injection at every point in that sequence,
plus a post-crash full-file verification. `just sim --scenario crash-matrix`.

### I-2 — Two segments never write to the same byte offset

**Assertion:** the set of `InProgress` intervals is pairwise disjoint, and disjoint from the
set of `Complete` intervals, at every observable moment.

**Why:** overlapping writers produce interleaved garbage that no checksum-free resume can
detect. This is the failure mode that dynamic segmentation and work stealing make easy to
introduce, because segments are split and reassigned while transfers are live.

**Mechanism:** the interval map is the single allocator. A worker cannot write to an offset
it was not granted, and grants are non-overlapping by construction. Splits happen inside the
allocator, not in the worker.

**Proof:** `proptest` over the interval map (arbitrary sequences of grant / split / complete /
abandon must preserve disjointness and total coverage), plus a debug-build write barrier that
panics on an out-of-grant write.

### I-3 — Resume never splices two different representations

**Assertion:** a download resumes against a remote representation only if a strong validator
(`ETag`, or `Last-Modified` when the server offers nothing stronger) matches the one recorded
at the time the existing bytes were fetched. On mismatch, the engine stops and asks; it does
not continue, and it does not silently restart over the existing bytes.

**Why:** the file changed on the server between sessions. Continuing produces a file that is
half version A and half version B, of exactly the expected size. Every integrity check that
does not hash the content passes.

**Mechanism:** `If-Range` with the recorded strong validator on every resume request. A `200 OK`
where `206 Partial Content` was expected means the representation changed — treat it as a
hard stop, never as "the server sent the whole file, great".

**Proof:** corpus cases `etag-changed-midway`, `weak-etag-only`, `no-validator`,
`200-instead-of-206`.

### I-4 — A completed download is verified before it is named

**Assertion:** the target file is only moved to its final path after: total length matches
the expected length, the block map covers `[0, length)` with no gaps, and — when the server
supplied one — the content digest matches.

**Why:** a partial file with the final name is indistinguishable from a good one to the user
and to every other program on the system.

**Mechanism:** download to `<name>.dppart` in the target directory, verify, then atomic
rename. Never write directly to the final path.

**Proof:** corpus assertion applied to every case; simulation asserts no `.dppart` is ever
renamed with an incomplete block map.

### I-5 — Content-encoding never corrupts offset arithmetic

**Assertion:** ranged requests are issued with `Accept-Encoding: identity`, and any response
carrying a `Content-Encoding` other than `identity` for a ranged request is rejected rather
than written at the requested offset.

**Why:** the server compresses the response; the bytes on the wire no longer correspond to
the byte range that was asked for; the engine writes compressed bytes at a raw-file offset.
The download completes at the "right" size and the file is garbage.

**Mechanism:** explicit `identity` on ranged requests; a response-header check before the
first byte is accepted; `Content-Range` is validated against the requested range, not trusted.

**Proof:** corpus cases `gzip-on-range`, `content-range-mismatch`, `content-range-absent`.

---

## Transfer correctness

### I-6 — Range support is proven, never assumed

**Assertion:** the engine does not open a second connection until it has *observed* a valid
`206` with a correct `Content-Range` for a probe request. `Accept-Ranges: bytes` on a `HEAD`
is a hint and is never sufficient.

**Why:** `Accept-Ranges` is advisory in RFC 9110, servers lie, and CDN edges disagree with
origins. A server that says it supports ranges and then returns `200` with the full body for
each of eight connections downloads the file eight times and assembles nonsense.

**Mechanism:** the Capability Probe (`docs/03-transfer-engine-spec.md` §2) issues
`Range: bytes=0-0` and validates status, `Content-Range`, and total size before segmentation
is permitted.

**Proof:** corpus cases `accept-ranges-lies`, `head-differs-from-get`, `cdn-edge-disagrees`.

### I-7 — Concurrency is bounded by evidence, not by a user's optimism

**Assertion:** the engine increases concurrency only while additional workers measurably
increase aggregate throughput, and decreases it on `429`, `503`, connection resets, or
throughput regression. A user-configured maximum is a ceiling, never a target.

**Why:** "32 connections" on a per-IP-limited origin makes the download slower and gets the
user rate-limited or blocked. This is the single most common way download managers make
things worse while appearing aggressive.

**Mechanism:** the Adaptive Concurrency Controller (`docs/03-transfer-engine-spec.md` §3),
EWMA-based, with explicit back-off on server-signalled pressure.

**Proof:** simulation scenarios `per-ip-cap`, `per-connection-cap`, `429-storm`,
`throughput-plateau`; each asserts the controller settles at or below the true optimum.

### I-8 — Redirect and identity state is captured, not recomputed

**Assertion:** the final URL after the redirect chain, the full chain, the cookie context, the
referer, and the request headers that produced a successful transfer are persisted with the
download and reused on resume.

**Why:** signed URLs, session-bound CDNs, and hotlink protection. Re-resolving from the
original URL on resume gets a 403, and the naive reaction — restart from zero — throws away
the existing bytes.

**Mechanism:** the `DownloadIdentity` record (`docs/03-transfer-engine-spec.md` §5).

**Proof:** corpus cases `signed-url-expiry`, `referer-required`, `session-cookie-required`,
`redirect-chain-to-other-host`.

---

## Storage and recovery

### I-9 — The journal is append-only and self-validating

**Assertion:** recovery-journal records are appended, never rewritten in place, and every
record carries a checksum. A torn tail record is discarded on replay; it never corrupts the
records before it.

**Why:** a power cut during a journal write must degrade to "we lost the last few seconds of
progress", never to "the journal is unreadable and the download is lost".

**Mechanism:** fixed-header records with CRC32C over the framing and payload; replay stops at
the first record whose checksum does not verify.

**Proof:** `proptest` over truncated and bit-flipped journals — replay must always yield a
prefix-consistent state and never a panic.

### I-10 — Disk space is reserved before it is needed

**Assertion:** the target file is preallocated to its full length (sparse where the platform
supports it) before the first byte is written, and `ENOSPC` during transfer pauses the
download cleanly rather than truncating or corrupting.

**Why:** running out of disk at 97% is common. The engine must survive it in a resumable state.

**Mechanism:** `fallocate`/`FSCTL_SET_SPARSE` + `SetFileValidData` where available, with a
graceful fallback; explicit `ENOSPC` handling at the write boundary.

**Proof:** simulation scenario `disk-full-at-97pct`; asserts the download is resumable and
the block map is accurate after space is freed.

### I-11 — Metadata is forward-compatible

**Assertion:** every persisted structure — SQLite schema, journal record, IPC message —
carries a version, and the daemon refuses to open state written by a newer version rather
than misinterpreting it.

**Why:** the user downgrades, or runs two versions, and the older one reads a newer block map
as if it were its own format.

**Mechanism:** `schema_version` in SQLite `user_version`; a version byte in the journal header;
`protocol_version` in the IPC handshake.

**Proof:** round-trip tests plus an explicit "refuse newer" test per format.

---

## Process and IPC

### I-12 — The daemon outlives every UI

**Assertion:** no client disconnect — GUI close, CLI exit, browser shutdown, extension
uninstall — cancels, pauses, or alters an in-flight transfer.

**Why:** this is the entire reason for the daemon architecture. If closing the window stops
the download, the architecture bought nothing.

**Mechanism:** transfers are owned by the daemon's scheduler; clients hold subscriptions, not
transfer handles.

**Proof:** integration test that connects a client, starts a transfer, kills the client, and
asserts progress continues.

### I-13 — Untrusted input is validated at the boundary

**Assertion:** every message from the browser extension, the native-messaging host, or any
IPC client is validated against its schema before any field is used, and a malformed message
is rejected without side effects.

**Why:** the native host's stdin is reachable by anything the browser can be persuaded to
install. It is an attack surface, not a convenience channel.

**Mechanism:** schema validation at the IPC decode layer; the daemon has no code path that
consumes an unvalidated field.

**Proof:** `cargo-fuzz` target over the IPC decoder; corpus of malformed native-messaging frames.

### I-14 — Secrets never reach disk unencrypted or logs at all

**Assertion:** cookies, `Authorization` headers, proxy credentials, and signed-URL query
parameters are stored in the OS keyring, and are redacted in every log level including `trace`.

**Why:** a debug log pasted into a bug report should never hand over a session.

**Mechanism:** a `Secret<T>` newtype whose `Debug`/`Display` render `[redacted]`; a lint that
forbids logging the raw types; keyring-backed storage.

**Proof:** a test that runs a full download at `trace` level and greps the output for the
secret material.

---

## How to use this file

- Before proposing an engine, storage, or IPC change as done, run `/invariant-check` and walk
  the list. For each invariant, state either "not affected" or name the test that covers it.
- When you find a new failure mode that is not covered here, **add an invariant** in the same
  change that fixes it. This file is meant to grow.
- Never weaken an invariant to make a test pass. If an invariant is genuinely wrong, that is
  an ADR, not an edit.
