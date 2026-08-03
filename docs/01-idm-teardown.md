# IDM Teardown

**Status: Informative.** Background for the engine design. Nothing here is a contract.

**Method: black-box only.** Everything in this document comes from IDM's public
documentation, its observable behaviour against servers we control, and the public behaviour
of comparable tools. No decompilation, no disassembly, no binary analysis, no copied code.
This is a clean-room study of *what* IDM does, so we can decide independently *how* to do it.

---

## 1. What IDM actually publishes about its algorithm

IDM documents its segmentation approach directly. The published mechanics are:

1. **Segmentation is dynamic, not up-front.** Most accelerators divide the file into N pieces
   once, before the transfer starts. IDM does not: it allocates work as connections become
   available.
2. **A new connection takes half of the largest remaining segment.** When a connection becomes
   available, IDM finds the largest outstanding segment and splits it in half; the new
   connection starts from the midpoint.
3. **Finished connections are reused, not re-established.** A connection that completes its
   segment is reassigned to new work without repeating connect, TLS handshake, or login. If
   there is an unstarted segment, it takes it; otherwise it splits the largest one again and
   helps the slowest worker.
4. **Very small segments are not split further.** Below a threshold, splitting costs more in
   round-trips than it saves.
5. **Progress is checkpointed several times a minute**, so a stop, pause, or power loss
   resumes and assembles correctly.

That is the whole published algorithm, and it is not difficult to reimplement. It is
approximately: *a work-stealing scheduler over a byte-interval space with connection reuse and
a minimum split size.* A competent implementation is a few hundred lines.

**Conclusion: the algorithm is not the moat.**

---

## 2. Where the actual advantage is

If the algorithm is a page of text, twenty years of development went somewhere else. From
IDM's own support documentation and FAQ, the shape of it is visible:

### 2.1 Download context capture

IDM does not receive a URL. It receives a URL plus the browser's *context*: the referer, the
cookies for that session, POST data where applicable, and the authentication state. Its public
integration API accepts exactly these fields.

This is why IDM works on sites where copying the link into a plain downloader returns an HTML
error page. **This is the single highest-leverage feature to copy**, and it is a browser
integration problem, not an engine problem.

→ Downpour spec: `06-browser-integration-spec.md`, and `DownloadIdentity` in
`03-transfer-engine-spec.md` §5.

### 2.2 URL refresh without losing progress

IDM has a "refresh download address" flow: when a signed or session-bound URL expires
mid-transfer, the user re-triggers the download in the browser and IDM binds the *new* URL to
the *existing* partial file, keeping every byte already fetched.

This is a large practical advantage. Most alternatives, faced with a `403` on resume, either
fail or restart from zero.

→ Downpour goes further: the refresh is automatic where the browser extension is present, and
the compatibility check is on the representation (size + validator + digest), not on the URL.
See `03-transfer-engine-spec.md` §5.

### 2.3 The failure-mode corpus

The rest of the twenty years is a very large accumulated set of rules for servers that
misbehave. IDM's own support pages document symptoms like *"server sends an HTML page when I
try to resume"* — which is a specific, real pathology with a specific handling.

Multiply that by two decades of user reports across every CDN, proxy, appliance, and
misconfigured origin on the internet. **That corpus is the moat.** It cannot be reimplemented
by writing more code, because the knowledge is not in code — it is in knowing which cases exist.

→ Downpour's counter-strategy is `09-testing-strategy.md`: generate the pathology space
deliberately instead of waiting to encounter it.

### 2.4 Media detection

IDM detects media requests inside the browser and offers a download button on players, with
handling that varies by player and by site. It is explicit that some protected protocols are
not supported.

→ Downpour scope: `07-media-spec.md`. Non-DRM HLS and DASH only, no per-site scrapers in 1.0.

---

## 3. The taxonomy of things that go wrong

This section is the practical output of the teardown: a checklist of failure modes that the
engine must handle and the corpus must contain. It is derived from the HTTP RFCs, from IDM's
and other download managers' public issue trackers and support documentation, and from the
known behaviour of common CDNs and proxies.

### 3.1 Range and partial content

| Pathology | Correct behaviour |
| --------- | ----------------- |
| `Accept-Ranges: bytes` advertised but ranges not honoured | Detect at probe, fall back to single stream (I-6) |
| `200 OK` returned for a range request | Treat as "no range support"; never write the body at an offset |
| `Content-Range` absent from a `206` | Reject the response |
| `Content-Range` disagrees with the requested range | Reject the response, do not write |
| Total length in `Content-Range` differs from `Content-Length` of a prior probe | Re-probe; treat as representation change |
| `HEAD` reports different capabilities than `GET` | Trust only `GET` observations |
| CDN edge honours ranges, origin does not (or vice versa) | Bind to the observed final URL; re-probe after redirect changes |
| Multipart ranges (`multipart/byteranges`) returned | Not requested by us; reject if received |
| Range unit other than `bytes` | Reject |

### 3.2 Identity and validators

| Pathology | Correct behaviour |
| --------- | ----------------- |
| No `ETag` and no `Last-Modified` | Single-session download only; refuse cross-session resume (or resume with explicit user consent and a full re-verify) |
| Weak `ETag` (`W/"…"`) | Not usable for `If-Range`; treat as no strong validator |
| `ETag` changes mid-transfer | Hard stop. Never splice (I-3) |
| `ETag` differs between CDN edges for the same content | Detectable false positive; prefer size + digest agreement before declaring a change |
| `Last-Modified` in a non-standard format | Parse permissively, compare conservatively |

### 3.3 Length and framing

| Pathology | Correct behaviour |
| --------- | ----------------- |
| No `Content-Length` (chunked) | Single stream, unknown total, no segmentation |
| `Content-Length` lies (short) | Detect at completion, treat as truncation, do not rename |
| `Content-Length` lies (long) | Detect stall at EOF, report, do not hang forever |
| `Content-Encoding: gzip` on a ranged request | Reject (I-5) |
| `Transfer-Encoding` and `Content-Length` both present | RFC violation; treat framing as untrusted, single stream |

### 3.4 Session, auth, and URL lifetime

| Pathology | Correct behaviour |
| --------- | ----------------- |
| Signed URL expires mid-transfer | Pause, request refresh, rebind to new URL, keep bytes (§2.2) |
| Referer required | Persist and replay the referer that worked |
| Session cookie required | Persist cookie context with the download; refresh from browser on demand |
| Server returns an HTML login page with `200` instead of the file | Content-type / magic-byte sanity check before accepting bytes |
| `401` / `407` challenge mid-transfer | Re-authenticate through the stored credential, not by retrying blindly |
| Redirect chain to a different host | Record the chain; reapply the correct cookie scope per host |

### 3.5 Connection behaviour

| Pathology | Correct behaviour |
| --------- | ----------------- |
| Per-IP connection cap | More connections do not help; controller must detect the plateau and back off (I-7) |
| Per-connection bandwidth cap | More connections *do* help; controller must detect and scale up |
| `429 Too Many Requests` | Back off, honour `Retry-After`, reduce concurrency |
| Silent connection drop with no FIN | Idle-timeout detection per worker; re-issue the outstanding range |
| One segment served far slower than others | ETA-aware rebalancing, not naive halving (`03-transfer-engine-spec.md` §4) |
| Server closes after N bytes regardless of range | Detect the pattern, adapt the segment size |

### 3.6 Local environment

| Pathology | Correct behaviour |
| --------- | ----------------- |
| Disk full mid-transfer | Clean pause, resumable state (I-10) |
| Process killed mid-write | Journal replay yields a prefix-consistent state (I-1, I-9) |
| OS crash / power loss | Same, verified by crash injection |
| Filesystem does not support sparse files or preallocation | Degrade gracefully, never silently skip preallocation |
| Target filename collides | Deterministic disambiguation, never overwrite silently |
| Path too long / invalid characters (Windows) | Sanitise with a documented, reversible rule |

---

## 4. What IDM does *not* do that we can

| Opportunity | Why it is available now |
| ----------- | ----------------------- |
| HTTP/2 stream-parallel range requests | Multiple ranges as concurrent streams on one connection: one handshake, no per-connection cap penalty |
| HTTP/3 / QUIC | No head-of-line blocking across streams; better on lossy links; connection migration across network changes |
| DNS `HTTPS`/`SVCB` records (RFC 9460) | Learn that an origin speaks h3 *before* connecting, instead of paying a round trip to discover `Alt-Svc` |
| TLS 1.3 0-RTT on resume | Cheaper reconnection after pause |
| `Repr-Digest` / `Content-Digest` (RFC 9530) | Server-supplied integrity we can verify, when offered |
| BLAKE3 block hashing | Fast enough to hash every block without becoming the bottleneck, enabling true per-block verification |
| `io_uring` (Linux) / IOCP (Windows) | Write path that does not become the constraint at multi-gigabit speeds |
| Deterministic simulation testing | Correctness that is reproducible by seed rather than by luck |

These are the substance of "research the technologies that exist now instead of copying the
old approach". They are enumerated with current status and risk in `14-tech-radar-2026.md`.

---

## 5. Reference implementations worth studying

Legitimate, open-source, and instructive. Read them for approach, not to copy code — and note
their licences before borrowing anything.

| Project | Licence | Worth studying for |
| ------- | ------- | ------------------ |
| **aria2** | GPL-2.0+ | Mature piece/segment management; multiple documented piece-selection strategies (default / in-order / geometric / random) and *why* each exists |
| **AB Download Manager** | Apache-2.0 | The closest product-shaped competitor; browser extension design; multi-platform packaging |
| **gopeed** | GPL-3.0 | Modern Go engine, clean plugin/extension model |
| **XDM** | GPL-2.0 | Dynamic segmentation and connection reuse in an open codebase |
| **Persepolis** | GPL-3.0 | Its issue history is a free catalogue of exactly the bugs to avoid — over-100% downloads, header handling, corruption on pause |
| **curl / libcurl** | curl licence | The most complete compatibility knowledge in the open-source world, in documented form |
| **yt-dlp** | Unlicense | The reference for media-site handling and for how large that problem really is |

> **Licence caution:** aria2, gopeed, XDM, and Persepolis are GPL. Downpour is Apache-2.0.
> Study the design; do not copy code, structure-by-structure translations, or test data from
> a GPL project into this repository.

---

## 6. Takeaways that shape the design

1. The segmentation algorithm is public and modest. Reimplement it, then improve it with
   ETA-awareness (`03-transfer-engine-spec.md` §4).
2. Browser context capture is the highest-value feature. It is Stage 7, and it is not optional.
3. URL refresh is the second-highest. Automate what IDM does manually.
4. The failure-mode corpus is the real work. Start it in Stage 2 and never stop growing it.
5. The protocol generation has changed. That is the opening, and it is why "build a modern
   equivalent" is a different project from "clone a 2005 architecture".
