# Glossary

Shared vocabulary. Use these terms exactly; ambiguity in naming becomes ambiguity in code.

---

## Downpour terms

| Term | Meaning |
| ---- | ------- |
| **Block** | A unit of data whose completion is recorded in the journal with a BLAKE3 hash. Not the same as a segment. |
| **Capability Probe** | The ranged `GET` that discovers what a server will actually allow. Never trusts advertisement. |
| **Compatibility corpus** | `tests/corpus/` — declarative cases describing a server pathology and the required engine behaviour. The project's primary asset. |
| **Controller** | The Adaptive Concurrency Controller. Decides how many workers to run, based on measurement. |
| **Coverage** | The set of byte intervals marked `Complete` for a download. |
| **`.dppart`** | The partial file. Sparse, preallocated, renamed to the final name only after verification. |
| **`.dpj`** | The append-only recovery journal for one download. |
| **Grant** | An allocation of a byte interval from the Segment Allocator to one worker. A worker may only write within its grant. |
| **Identity** | `DownloadIdentity` — what makes a download the same download across URL changes. Size, validator, digest, request context. |
| **Interval map** | The data structure owning `[0, total_length)` as disjoint `Pending`/`InProgress`/`Complete` intervals. |
| **Journal** | See `.dpj`. Source of truth for progress; SQLite is a checkpoint. |
| **Pathology server** | The local test server that enacts corpus cases. Serves deterministic content from a seed. |
| **Redundant fetch** | Fetching a range already in progress on a slow worker, in the tail window only. First completion wins. |
| **Refresh** | Rebinding a download to a new URL without losing bytes, after the old URL expired. |
| **Segment** | A contiguous byte interval assigned to one worker. Dynamic — split and reassigned during the transfer. |
| **Silent corruption** | The file is wrong and nothing reports it. The most severe defect class; a release blocker. |
| **Splice** | Combining bytes from two different representations into one file. Must never happen (I-3). |
| **Stage gate** | The wall at the end of each roadmap stage. Objectively checkable exit criteria + human approval. |
| **Tail** | The last few percent of a download, where naive schedulers lose time. |
| **Work stealing** | An idle worker taking part of a busy worker's remaining range. Ours is ETA-aware, not naive halving. |

## HTTP terms

| Term | Meaning |
| ---- | ------- |
| **`Accept-Ranges`** | Advisory header claiming range support. Advisory. Never sufficient (I-6). |
| **`Alt-Svc`** | Response header advertising an alternative protocol (usually h3). Costs a round trip to discover; DNS `HTTPS` records avoid it. |
| **`Content-Digest` / `Repr-Digest`** | RFC 9530 integrity fields. `Content-Digest` covers the transmitted bytes; `Repr-Digest` covers the representation before encoding. |
| **`Content-Range`** | Response header describing which bytes a `206` contains and the total size. Validated, never trusted. |
| **`If-Range`** | Conditional making a range request contingent on a validator. The mechanism that makes resume safe. |
| **Representation** | The specific version of a resource. A URL may serve different representations over time — which is exactly the resume hazard. |
| **Strong validator** | An `ETag` without the `W/` prefix. Usable with `If-Range`. A weak ETag is not. |
| **`206 Partial Content`** | The success status for a range request. |
| **`416 Range Not Satisfiable`** | The requested range is outside the representation. Usually means our size information is stale. |

## Protocol terms

| Term | Meaning |
| ---- | ------- |
| **ALPN** | TLS extension negotiating the application protocol (`http/1.1`, `h2`, `h3`). |
| **BDP** | Bandwidth-Delay Product. Bandwidth × RTT — the amount of data in flight on a saturated link. Determines the flow-control window needed. |
| **Head-of-line blocking** | One stalled item blocking the ones behind it. Per-connection in HTTP/1.1, at the TCP layer in HTTP/2, absent across streams in HTTP/3. |
| **QUIC** | The UDP-based transport under HTTP/3. Multiplexed streams, connection migration, 0-RTT. |
| **`SVCB` / `HTTPS` records** | RFC 9460 DNS records carrying service parameters, including `alpn`. Lets a client know an origin speaks h3 before connecting. |
| **0-RTT** | Sending application data in the first flight on resumption. Fast; only safe for idempotent requests. |

## Testing terms

| Term | Meaning |
| ---- | ------- |
| **Crash injection** | Deliberately terminating the process at a chosen point to verify recovery. |
| **Deterministic simulation** | Running against a simulated clock and network so a seed reproduces an execution exactly. |
| **Property test** | Asserting an invariant over generated inputs rather than checking specific examples. |
| **Seed** | The number determining a simulation run. A failing seed is a permanent, exactly reproducible test case. |

## Project terms

| Term | Meaning |
| ---- | ------- |
| **ADR** | Architecture Decision Record. `docs/adr/NNNN-*.md`. Written before the code, with a reversal trigger. |
| **Harness** | `docs/agent/HARNESS.md` — the operating manual for agents working on this repository. |
| **Invariant** | A property in `docs/agent/INVARIANTS.md` that must always hold. Violations block releases. |
| **Scorecard** | The weighted table in `00-vision-and-scorecard.md` defining what 9.5/10 means. |
