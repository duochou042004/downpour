# ADR-0016: Host the storage-backed sink in `downpour-http` until `downpour-engine` lands

- **Status:** accepted
- **Date:** 2026-08-05
- **Stage:** S2
- **Deciders:** maintainer (proposed by Claude Code)

## Context

S2-T8 replaces S1's minimal file sink with `downpour-storage`, closing B-5. The task note is
explicit: *remove the minimal sink only after all callers use the storage crate; do not keep
two competing durability paths.* Two durability paths is precisely the state in which a fix
applied to one is silently absent from the other, which is how I-1 gets violated by omission
rather than by a bad decision.

The obstacle is the dependency direction in `docs/02-architecture.md` §3:

```
types ◀── intervals ◀── engine ──▶ http ──▶ types
              ▲            │
              │            ▼
              └───────── storage ──▶ types

daemon ──▶ engine, storage, ipc, http
cli, host, gui ──▶ ipc, types      (never engine, never storage)
```

`http` and `storage` are siblings. Neither depends on the other; `downpour-engine` is the
crate that sees both and wires them together. `downpour-engine` does not exist — the roadmap
places it in S3, and its actual content (scheduler, segment allocator, concurrency controller,
worker pool) is S3 and S4 work.

So S2-T8 needs a home for one adapter: something that implements `downpour-http`'s
`SinkTarget` on top of `downpour-storage`'s `PartFile`, `JournalFile`, `DurableWriter`, and
`IntervalMap`. No crate is currently allowed to see both.

Two facts make this less alarming than it first looks. First, `SingleStream` — the download
orchestration that would consume such an adapter — *already* lives in `downpour-http` as an
acknowledged temporary resident. Its own module header says orchestration belongs in
`downpour-engine`, and backlog **B-4** records the move. Second, `downpour-cli` links
`downpour-http` directly for the same reason, also recorded in B-4. The layering violation
S2-T8 needs is not a new one; it is the same one, one crate deeper.

There is a second, independent question. `DurableWriter` is synchronous and its commit point
is two `fsync` calls. `SinkTarget` is `async_trait`. `.claude/rules/rust.md` requires blocking
filesystem work to go through `spawn_blocking` or a dedicated thread, because blocking the
executor stalls every transfer in the process. `docs/04-storage-and-recovery-spec.md` §2.2
prescribes the eventual shape: *one file handle per download, owned by the writer task;
workers send `(offset, bytes)` over a bounded channel.* That shape presupposes many workers,
which S2 does not have.

## Options

### Where the adapter lives

| Option | Pros | Cons |
| ------ | ---- | ---- |
| **`downpour-http` depends on `downpour-storage`; adapter sits beside `SingleStream`** | One durability path immediately; no new crate; the coupling is the same one B-4 already records, and both pieces leave together in S3 | Adds an arrow the architecture diagram does not have, for the length of S2 |
| Create `downpour-engine` now to host the adapter | Respects the diagram exactly | Crosses the S3 stage boundary, which rule 2 forbids. An engine crate holding only a sink adapter is a shell whose shape would constrain the scheduler and allocator design that is the crate's actual reason to exist |
| `downpour-storage` implements `SinkTarget`, depending on `downpour-http` | Also gives one durability path | Inverts the arrow the diagram most wants to keep. Storage must stay usable by daemon startup recovery with no HTTP present; making the durability crate depend on the protocol crate is the harder coupling to remove, not the easier one |
| Move `SinkTarget` into `downpour-types` | Both crates could then see the trait | `downpour-types` is specified as no I/O and no async, and that purity is what makes it property-testable. It also buys nothing: the concrete adapter still needs both crates |
| Keep the minimal sink for existing callers, add storage alongside | No layering change | Two competing durability paths — the exact outcome B-5 and the task note forbid |

### How async transfer code drives the blocking writer

| Option | Pros | Cons |
| ------ | ---- | ---- |
| **Move the writer into `spawn_blocking` per call and back out** | Smallest correct change; the writer stays single-owner by construction, since it is physically moved rather than shared; no channel, no thread lifecycle, no shutdown ordering | One `spawn_blocking` per chunk. Real but unmeasured cost, and pure overhead once a worker pool exists |
| Dedicated writer thread behind a bounded channel | The shape §2.2 prescribes; amortises the handoff | Builds the S3 worker-pool boundary in S2 with one worker to serve. Shutdown, error propagation, and backpressure semantics would be designed against a single-stream case and then redesigned against the real one |
| Block the executor and accept it | Trivial | Stalls every transfer in the process on each `fsync`. Not an option under the concurrency rules |

## Decision

**Add `downpour-storage` to `downpour-http`'s dependencies and place the storage-backed
`SinkTarget` beside `SingleStream`, which already lives there for the same reason. Delete
`FileTarget`, the S1 minimal sink, in the same change.**

The adapter owns one `PartFile`, one `JournalFile`, one `DurableWriter`, and one `IntervalMap`
for the download. It grants the whole representation to a single worker identity, stages each
accepted chunk, and flushes on the writer's existing byte and time bounds. I-1's ordering is
not reimplemented: it stays exactly where S2-T5 put it, inside `DurableWriter::flush`.

**Drive the writer with `tokio::task::spawn_blocking`, moving it into the closure and back out
on every call.** The bounded-channel writer task from §2.2 arrives in S3 together with the
worker pool that makes it necessary. Correctness is identical between the two; only the
handoff cost differs, and `docs/agent/HARNESS.md` is explicit that optimisation waits for the
correctness gate.

This ADR does not move `SingleStream`, does not introduce resume, and does not add digest
verification before the rename. Those are S2-T9, S2-T11, and B-4 respectively.

## Consequences

**Easier:** There is exactly one durability path in the workspace. Every download — corpus
case, gigabyte case, future daemon transfer — goes through preallocation, positional writes,
and the journal ordering, so a defect in that path is found by the whole corpus rather than by
whichever tests happen to exercise the storage crate directly. B-5 closes. `.dppart` creation
becomes exclusive (`O_EXCL`), so two processes racing the same target now collide loudly
instead of interleaving writes.

**Harder:** `downpour-http` briefly carries a dependency the architecture diagram does not
show. The mitigation is that it is scheduled to leave with `SingleStream`, not left to be
rediscovered: this ADR and B-4 both name the move, and B-21 records the reversal explicitly.
Preallocation also means the `.dppart` now reaches full length at creation, so a failed
download leaves a full-length sparse file rather than a short one; the recovery path from
S2-T7 already reads such a file correctly, because coverage comes from the journal and never
from the file's length.

**Accepted:** one `spawn_blocking` per accepted chunk. On a 1 GB transfer with 64 KiB chunks
that is roughly sixteen thousand handoffs. This is measured on the gigabyte corpus case rather
than assumed, and the number is recorded in `state/progress.json` so the S3 change has a
baseline to beat.

**Accepted:** a download whose representation length the server never stated keeps its bytes
durable but gets no journal and no interval map. Measured on the corpus, `total_length` is
`None` only when the server offers neither range support nor a declared length — chunked or
close-delimited framing with `Accept-Ranges` absent. Every existing case still yields a length,
because a validated `Content-Range` from the capability probe supplies one even when the body
framing does not.

For that residual case, all three pieces of the range machinery are impossible rather than
merely inconvenient: preallocation has no length to reserve (I-10), the journal header has no
`total_length` to bind, and the interval map has no `[0, total)` to partition. Resume is
impossible too, by construction — we only arrive here because ranges were not proven, so
there is nothing a journal could ever be replayed *for*. Such a transfer therefore uses the
same part file and the same durability sync with the journal and the map absent.

This is deliberately not a second durability path. It is one sink type with the range
machinery necessarily absent, and the choice is made in exactly one place so a change to the
ordering cannot reach one arm and miss the other. The download is recorded as non-resumable;
S2-T9 makes the refusal explicit rather than leaving it implied.

## Reversal trigger

Delete `downpour-http`'s dependency on `downpour-storage` when `downpour-engine` lands in S3
and `SingleStream` moves there with the adapter. That is the expected end, not a contingency;
if S3 closes with the dependency still present, it has been forgotten and B-21 is the record
that says so.

Replace `spawn_blocking` with the §2.2 writer task when either the gigabyte corpus case shows
a measurable regression against its S1 baseline, or the worker pool lands and a second worker
needs the same file handle. Do not replace it earlier on the strength of the argument alone —
the reason to prefer a channel is amortisation across workers, and with one worker there is
nothing to amortise.

## Resolution

The crate-boundary reversal trigger fired in S3-T8. `SingleStream` and `StorageSink` moved together
to `downpour-engine`; `downpour-http` no longer has production dependencies on either
`downpour-storage` or `downpour-intervals`. A recursive production-graph test in each affected
client/protocol crate makes both boundaries executable rather than relying on this note.

The second trigger is tracked independently by B-23. The fixed segmented worker path uses S3's
bounded writer actor, while the retained single-stream fallback still uses the per-call
`spawn_blocking` adapter described above. This resolution does not claim that separate performance
cleanup is complete.
