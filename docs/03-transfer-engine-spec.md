# Transfer Engine Specification

**Status: Normative.**

The engine is the part that can be silently wrong. Read `docs/agent/INVARIANTS.md` alongside
this document; every section here exists to uphold one or more of them.

---

## 1. Lifecycle of a download

```text
  Submitted
     │
     ▼
  Probing ──────────────▶ Failed (unrecoverable probe error)
     │
     ▼
  Planned            (RemoteObject known; strategy chosen)
     │
     ▼
  Transferring ◀──▶ Paused ◀──▶ Stalled ◀──▶ AwaitingRefresh
     │                                              │
     │◀─────────────────────────────────────────────┘
     ▼
  Verifying
     │
     ▼
  Completed
```

| State | Meaning |
| ----- | ------- |
| `Submitted` | Accepted, queued, nothing sent |
| `Probing` | Capability probe in flight |
| `Planned` | `RemoteObject` known; concurrency strategy selected |
| `Transferring` | One or more workers active |
| `Paused` | User-initiated; all grants released, journal flushed |
| `Stalled` | No progress for the stall window; retrying with back-off |
| `AwaitingRefresh` | URL expired or auth failed; needs a fresh URL or credential |
| `Verifying` | All bytes present; checking length, coverage, digest |
| `Completed` | Verified and renamed to the final path |
| `Failed` | Unrecoverable; the partial file and journal are retained |

**Rule:** the transition into `Completed` is the only place the final rename happens, and it
happens only after verification (I-4).

---

## 2. Capability Probe

The probe answers: *what will this server actually let us do?* It never trusts advertisement.

### 2.1 Procedure

1. Resolve the origin. If a DNS `HTTPS`/`SVCB` record (RFC 9460) is available, read its `alpn`
   parameter to learn supported protocols before connecting. This can skip the `Alt-Svc`
   discovery round trip for HTTP/3.
2. Issue a **ranged GET**, not a `HEAD`:

   ```http
   GET /path HTTP/1.1
   Range: bytes=0-0
   Accept-Encoding: identity
   ```

   `HEAD` is used only as a supplementary signal. Servers routinely answer `HEAD` differently
   from `GET`; only `GET` observations are authoritative.
3. Follow redirects, recording the entire chain and the final URL.
4. Validate the response before concluding anything:

   | Check | Failure means |
   | ----- | ------------- |
   | Status is `206` | No usable range support → single stream |
   | `Content-Range` present and syntactically valid | Reject; single stream |
   | `Content-Range` matches the requested range | Reject the response entirely (do not write) |
   | Total length parsed from `Content-Range` | Unknown length → single stream, no segmentation |
   | `Content-Encoding` is absent or `identity` | Reject (I-5) |
   | Body is exactly 1 byte | Server ignoring ranges → single stream |

5. Record validators: strong `ETag` preferred, `Last-Modified` as fallback. A weak ETag
   (`W/"…"`) is **not** usable for `If-Range` and is recorded as "no strong validator".
6. Record `Repr-Digest` / `Content-Digest` (RFC 9530) if offered — free end-to-end integrity.
7. Sanity-check the content: if the response looks like an HTML error or login page
   (content-type, or magic bytes) while a binary was expected, do not begin the transfer.
   Report it as a session/auth problem. This is the *"server sends an HTML page"* pathology.

### 2.2 Output

```rust
pub struct RemoteObject {
    pub final_url: Url,
    pub redirect_chain: Vec<Url>,
    pub total_length: Option<u64>,
    pub range_support: RangeSupport,      // Proven | Absent | Unknown
    pub validator: Validator,             // StrongETag | LastModified | None
    pub digest: Option<ContentDigest>,
    pub protocol: NegotiatedProtocol,     // Http1 | Http2 | Http3
    pub suggested_filename: Option<String>,
    pub content_type: Option<Mime>,
    pub probed_at: SystemTime,
}
```

`range_support: Proven` is only ever set by an observed, validated `206`. Nothing else sets it.

### 2.3 Re-probe triggers

- The final URL changed (redirect target moved).
- A worker received a status inconsistent with the recorded capabilities.
- Resume after any interval longer than the configured freshness window.
- The user supplied a refreshed URL.

---

## 3. Adaptive Concurrency Controller

The controller decides how many concurrent transfers to run and of what kind. It replaces
IDM's fixed connection count with a measured one (I-7).

### 3.1 Principle

Increase concurrency while it demonstrably increases aggregate throughput. Decrease it on
server-signalled pressure or throughput regression. The user's setting is a **ceiling**, never
a target.

### 3.2 Signals

| Signal | Source | Use |
| ------ | ------ | --- |
| Per-worker throughput (EWMA) | Byte counters | Detect plateau and regression |
| Aggregate throughput (EWMA) | Sum | The objective function |
| RTT and handshake time | Connection events | Cost of adding a worker |
| `429`, `503`, `Retry-After` | Response | Hard back-off signal |
| Connection resets / timeouts | Transport | Back-off signal |
| Disk write throughput and queue depth | Storage layer | Do not add network capacity the disk cannot absorb |
| HTTP/2 or /3 flow-control stalls | Protocol backend | Prefer more streams vs. more connections |

### 3.3 Algorithm

Deliberately simple and explainable. No machine learning in 1.0.

```text
state: n = 1 worker, phase = PROBE_UP

every DECISION_INTERVAL (default 2s, at least 3 EWMA windows):

  if server_pressure_signal:        # 429 / 503 / resets
      n ← max(1, floor(n * 0.5))
      phase ← BACKOFF, cooldown ← 30s
      continue

  if disk_saturated:
      hold n, do not increase
      continue

  gain ← (throughput_now - throughput_at_last_step) / throughput_at_last_step

  match phase:
    PROBE_UP:
       if gain > GAIN_THRESHOLD (default 0.10) and n < ceiling:
           n ← n + step        # step: 1 → 2 → 4 → 8, then +2
       else:
           phase ← HOLD
    HOLD:
       if throughput dropped > 15% for 2 intervals:
           n ← max(1, n - 1)
       if idle for HOLD_REPROBE (default 60s):
           phase ← PROBE_UP    # conditions may have changed
    BACKOFF:
       after cooldown: phase ← PROBE_UP
```

### 3.4 Protocol-aware choice of *what* to add

Adding capacity does not always mean adding a connection.

| Negotiated protocol | Add capacity by | Escalate to a new connection when |
| ------------------- | --------------- | --------------------------------- |
| HTTP/1.1 | New connection (one range each) | Always — this is the only option |
| HTTP/2 | New stream on the existing connection | Streams plateau *and* server appears to cap per-connection bandwidth |
| HTTP/3 | New QUIC stream | Same test as HTTP/2; also consider a second connection over a different local interface if multi-homed |

This is the concrete form of *"32 sockets is no longer automatically the right answer"*.
Full decision table: `05-protocol-matrix.md`.

### 3.5 Explainability

Every decision is recorded with its inputs and its reason. `dp explain <id>` renders them:

```
t+04.0  n=2→4   PROBE_UP   gain=+31%   rtt=42ms  disk=18%   reason=throughput-scaling
t+06.0  n=4     HOLD       gain=+2%    rtt=44ms  disk=19%   reason=plateau
t+21.0  n=4→2   BACKOFF    429 from cdn.example.com, Retry-After=10
```

Non-negotiable. An adaptive controller that cannot explain itself cannot be debugged, and
this one will need debugging.

---

## 4. Segment Allocator

Owns the byte-interval space for one download. **It is the sole allocator** — workers never
choose their own offsets (I-2).

### 4.1 Model

The file is a set of disjoint intervals over `[0, total_length)`, each in one state:

```rust
pub enum IntervalState {
    Pending,                 // not yet granted
    InProgress { worker: WorkerId, granted_at: Instant },
    Complete,                // written AND durable (I-1)
}
```

Backed by an interval tree / range map. Invariants asserted after **every** mutation, in debug
builds and in property tests:

- All intervals are disjoint.
- Their union is exactly `[0, total_length)`.
- No zero-length interval exists.
- Adjacent intervals in the same state are merged.

### 4.2 Grant

A worker asking for work gets, in priority order:

1. The largest `Pending` interval, if one exists.
2. Otherwise, a split of the `InProgress` interval with the **worst projected finish time**
   (§4.3), provided the remainder exceeds `MIN_SPLIT_BYTES`.
3. Otherwise, nothing — the worker retires. The transfer is in its tail.

`MIN_SPLIT_BYTES` default 1 MiB, tunable. Below it, a split costs more in round-trips and
handshakes than it saves. This mirrors IDM's documented "do not split tiny segments" rule.

### 4.3 ETA-aware splitting (the improvement over IDM)

IDM halves the largest remaining segment. Halving is correct only when all workers have the
same throughput — which is exactly what is not true across CDN edges, congested paths, and
mixed link types.

Instead, split to **equalise projected finish times**.

For worker `i` on interval with `r_i` bytes remaining at rate `v_i`:

```
ETA_i = r_i / v_i
```

Choose the interval with the largest `ETA`. Let its worker's rate be `v_slow` and the
requesting worker's expected rate `v_new` (its own EWMA, or the pool median if it is new).
Split the remaining `r` bytes so both finish together:

```
r_slow = r · v_slow / (v_slow + v_new)
r_new  = r · v_new  / (v_slow + v_new)
```

Worked example: 400 MB remaining on a worker doing 10 MB/s; a free worker measured at 30 MB/s.
Halving gives 200/200 → the slow worker takes 20 s, the fast one 6.7 s, and the transfer waits
13 s for the tail. ETA-splitting gives 100/300 → both finish at 10 s. **A 33% improvement on
the tail from arithmetic alone.**

Guards:

- Never split below `MIN_SPLIT_BYTES`.
- Never split an interval whose worker has been active for less than `MIN_RATE_SAMPLES`
  (default 3 EWMA windows) — its rate estimate is not yet meaningful; fall back to halving.
- Cap the split ratio at 9:1 to limit damage from a bad rate estimate.

### 4.4 Completion and the durability boundary

A worker reports bytes written. The allocator marks `Complete` **only after** the storage
layer confirms durability (I-1). The ordering is fixed and is not an optimisation target:

```
worker writes bytes at offset
   → storage flushes to stable storage
      → journal record appended
         → allocator marks Complete
```

Reversing any two of these reintroduces the classic corruption bug.

### 4.5 The tail problem

The last few percent of a download is where naive schedulers lose. Rules:

- When total remaining < `TAIL_THRESHOLD` (default 8 MiB), stop granting new splits.
- Enable **redundant fetch**: an idle worker may re-fetch a range already `InProgress` on a
  demonstrably slow worker; first completion wins, the loser is cancelled. This costs
  bandwidth and is therefore off by default and only ever used inside the tail window.
- Never allow redundant fetch to violate I-2: both fetches write to a *staging buffer*; only
  the winner's buffer is committed to the file.

---

## 5. Download Identity and URL Refresh

A download is not identified by its URL. URLs expire; the file does not.

### 5.1 Identity record

```rust
pub struct DownloadIdentity {
    pub id: DownloadId,
    pub expected_name: String,
    pub total_length: Option<u64>,
    pub validator: Validator,
    pub server_digest: Option<ContentDigest>,   // RFC 9530, if offered
    pub local_prefix_hash: Option<Blake3Hash>,  // hash of the first N MiB we hold
    pub origin: Origin,
    pub page_url: Option<Url>,                  // where the user found it
    pub request_context: RequestContext,        // referer, cookies, headers, method, body
    pub current_url: Url,
    pub url_history: Vec<(Url, SystemTime)>,
}
```

`request_context` is what makes downloads work on sites where a bare URL returns an error
page. It comes from the browser extension (`06-browser-integration-spec.md`) and it is
persisted, so resume replays the request that actually worked (I-8).

### 5.2 Refresh flow

When a transfer fails with `403`, `401`, `410`, or a sudden HTML body where binary was
expected, the engine does not fail and does not restart:

1. Move to `AwaitingRefresh`. Keep every byte. Release grants.
2. Ask for a fresh URL:
   - **Automatic:** the extension re-triggers the download on the origin page and hands back
     the new URL with its context.
   - **Manual:** the user pastes a new URL (`dp refresh <id> <url>`).
3. Probe the new URL and check **compatibility** with the existing partial file:

   | Check | Weight |
   | ----- | ------ |
   | Total length identical | Required |
   | Strong validator identical | Strong evidence, sufficient |
   | Server digest identical | Strong evidence, sufficient |
   | Re-fetched sample range byte-identical to what we hold | Strong evidence, sufficient |
   | Filename and content-type identical | Weak, supporting only |

   Compatible → rebind and continue from the existing bytes.
   Incompatible → **stop and ask**. Never splice (I-3). Never silently restart, either —
   the user may prefer to keep the partial file.

4. Record the new URL in `url_history` and update `current_url`.

The sample-range re-fetch in step 3 is what makes this safe when the server offers no strong
validator: fetch 64 KiB from the middle of what we already hold and compare. Cheap, and it
catches the case that would otherwise corrupt.

---

## 6. Protocol backends

The engine sees only the `TransferProtocol` trait (`02-architecture.md` §2.3). Backends:

| Backend | Stage | Basis |
| ------- | ----- | ----- |
| `h1h2` | S1 | Production. `reqwest`/`hyper` + `rustls`. |
| `h3` | S6 | Experimental, feature-gated, off by default. `h3` + `quinn`. |
| `sim` | S2 | Deterministic simulation. Not compiled into release binaries. |
| `curl` | Contingency only | If and only if the corpus proves a pure-Rust stack cannot handle a class of proxy/auth (NTLM, Kerberos, exotic appliances). Requires an ADR. |

The `curl` row is a deliberate release valve. The goal is a 9.5/10 product, not a purity
score. But it is a contingency backed by corpus evidence, not a default.

---

## 7. Retry and back-off

| Condition | Response |
| --------- | -------- |
| Connection reset / timeout | Return the range to the allocator; retry with jittered exponential back-off (base 500 ms, cap 30 s) |
| `429` / `503` with `Retry-After` | Honour it exactly. Reduce concurrency. Do not retry early. |
| `429` / `503` without `Retry-After` | Exponential back-off; halve concurrency |
| `5xx` other | Retry up to `MAX_RETRIES` (default 5), then mark the download `Stalled` |
| `4xx` other than 401/403/408/410/429 | Do not retry; unrecoverable for this URL |
| `401` / `403` / `410` | → `AwaitingRefresh` (§5.2) |
| Truncated response body | Return the unwritten remainder to the allocator; count against the retry budget |

Back-off is per-origin, not per-worker. Five workers each retrying independently against a
rate-limited origin is a self-inflicted denial of service.

---

## 8. Rate limiting

- Global limit, per-download limit, and per-schedule limit (e.g. throttle during work hours).
- Token bucket, applied at the read side so back-pressure propagates to the server rather
  than buffering locally.
- **Interaction with the controller:** when a rate limit is the binding constraint, the
  controller must recognise it and stop probing upward. Otherwise it reads its own throttle
  as a server plateau and keeps adding useless workers. This is an easy bug and it must have
  a dedicated test.

---

## 9. Parameters

Defaults live here; all are configurable. Changing a default requires a benchmark showing why.

| Parameter | Default | Notes |
| --------- | ------: | ----- |
| `MIN_SPLIT_BYTES` | 1 MiB | Below this, splitting costs more than it saves |
| `TAIL_THRESHOLD` | 8 MiB | Where tail handling engages |
| `DECISION_INTERVAL` | 2 s | Controller cadence |
| `GAIN_THRESHOLD` | 0.10 | Minimum throughput gain to justify another worker |
| `MAX_WORKERS` | 16 | User ceiling; the controller usually settles far below |
| `STALL_WINDOW` | 30 s | No progress → `Stalled` |
| `MAX_RETRIES` | 5 | Per range, per attempt cycle |
| `EWMA_ALPHA` | 0.3 | Throughput smoothing |
| `PROBE_TIMEOUT` | 15 s | Capability probe |
| `CAPABILITY_FRESHNESS` | 15 min | How long §2.3's evidence stays usable without re-probing |
| `JOURNAL_FLUSH_INTERVAL` | 2 s or 8 MiB | Whichever comes first (`04-storage-and-recovery-spec.md`) |

`CAPABILITY_FRESHNESS` is bounded from both directions. Retry back-off caps at 30 s over at most
`MAX_RETRIES` attempts (§7), so a full in-call retry cycle is a couple of minutes — comfortably
inside the window, which is what stops an ordinary retry from re-probing and thereby replacing the
recorded validator with one fetched now. A download resumed after a genuine pause is outside it,
which is the case §2.3's third trigger exists for: signed URLs expire, CDN edges rotate, and files
get replaced while nobody is watching.

`MAX_WORKERS` defaulting to 16 rather than IDM's 32 is intentional: with HTTP/2 and HTTP/3,
capacity is added as streams, and a high connection ceiling mostly serves to get users
rate-limited. The controller is what determines the actual number.
