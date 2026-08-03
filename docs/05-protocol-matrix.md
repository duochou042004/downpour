# Protocol Matrix

**Status: Normative.**

IDM's architecture assumes HTTP/1.1: to get parallelism you open more TCP connections. That
assumption is now wrong often enough to matter. This document defines what the engine does
per protocol and per server behaviour.

---

## 1. The central change since IDM

| | HTTP/1.1 | HTTP/2 | HTTP/3 |
| --- | --- | --- | --- |
| Parallelism unit | Connection | Stream | Stream |
| Cost of more parallelism | TCP + TLS handshake each | ~free | ~free |
| Head-of-line blocking | Per connection | At the TCP layer (whole connection) | None across streams |
| Typical server connection cap | 6–8 per client | 1 needed | 1 needed |
| Flow control | TCP only | Per stream + per connection | Per stream + per connection |
| Recovery from packet loss | Stalls the connection | Stalls the connection | Stalls only the affected stream |
| Connection migration | No | No | Yes (NAT rebind, Wi-Fi → cellular) |
| 0-RTT resumption | No | TLS 1.3 only | Yes |

**Consequence:** on an HTTP/2 or HTTP/3 origin, "open 16 connections" is not the fast path.
It is a way to look aggressive while paying 16 handshakes and possibly tripping a per-IP cap.
The right move is 16 *streams* on one connection, escalating to a second connection only when
evidence says the server caps bandwidth per connection.

---

## 2. Protocol selection

```
1. If a DNS HTTPS/SVCB record exists (RFC 9460) and lists h3 in alpn:
       attempt HTTP/3 first, with a fast fallback timer.
2. Else attempt HTTP/2 via ALPN.
3. If the response carries Alt-Svc advertising h3:
       record it for this origin; use h3 on the next connection, not this one.
4. Fall back to HTTP/1.1.
5. Cache the outcome per origin in compat_profiles.
```

The DNS step is a genuine, current advantage: it removes the round trip that `Alt-Svc`
discovery costs, which IDM's generation had no mechanism for.

**Fallback discipline:** an HTTP/3 attempt that has not completed its handshake within
`H3_RACE_TIMEOUT` (default 300 ms) loses to a parallel HTTP/2 attempt. QUIC is UDP, and UDP is
blocked or throttled on enough networks that a slow failure is a real risk. Race, do not wait.

---

## 3. Behaviour per protocol

### HTTP/1.1

- One outstanding range per connection. No pipelining (broken in practice).
- Keep-alive is mandatory for reuse — this is IDM's documented "reuse finished connections"
  advantage and it applies with full force here.
- Concurrency = connection count. The controller's step function is connections.
- Watch for servers that close after N requests; detect and pre-emptively rotate.

### HTTP/2

- One connection, many streams, one range per stream.
- Respect `SETTINGS_MAX_CONCURRENT_STREAMS`. Exceeding it gets streams refused, and a naive
  engine reads that as a transfer error.
- Tune the connection-level and stream-level flow-control windows upward for bulk transfer;
  defaults are sized for web pages, not for a 4 GB ISO, and an untuned window is a common
  cause of "HTTP/2 is slower than HTTP/1.1" measurements.
- TCP head-of-line blocking still applies: on a lossy path, one lost packet stalls every
  stream. If loss is detected and h3 is available, prefer h3.

### HTTP/3 / QUIC

- One connection, many streams. Loss affects only the stream that lost the packet.
- Stream and connection flow control both need raising for bulk transfer.
- 0-RTT on resume after a pause is a real latency win; only for idempotent GETs, and the
  replay-safety caveat is respected.
- Connection migration survives a network change without restarting transfers — valuable on
  laptops.
- **Status: experimental in the Rust ecosystem** (see `14-tech-radar-2026.md`). Feature-gated,
  off by default, behind the `TransferProtocol` trait so it cannot destabilise the scheduler.

---

## 4. Server-behaviour decision table

The row that matches determines strategy. Detection method is what the probe or the
controller observes — never what the server advertises.

| Observed behaviour | Detection | Strategy |
| ------------------ | --------- | -------- |
| Ranges proven, no caps | `206` + linear throughput scaling | Scale to the throughput plateau |
| Per-connection bandwidth cap | Aggregate scales linearly with connections; each stays flat | **More connections help.** Scale connections even on h2/h3 |
| Per-IP bandwidth cap | Aggregate flat as workers increase | Settle at the minimum worker count that reaches the cap |
| Per-IP connection cap | New connections refused or reset | Stay under the observed limit; prefer streams |
| `429` / `Retry-After` | Response | Halve concurrency, honour the delay exactly |
| No range support | `200` in response to `Range` | Single stream. Never fake multipart |
| Range advertised but not honoured | `Accept-Ranges: bytes` + `200` for a range | Treat as no range support (I-6) |
| No `Content-Length` (chunked) | Framing | Single stream, unknown total, no segmentation |
| Signed URL with expiry | `403`/`410` mid-transfer | → `AwaitingRefresh` (`03-transfer-engine-spec.md` §5) |
| Session-bound (cookies/referer) | HTML body where binary expected | Replay stored request context; refresh from browser |
| Redirect to a different host | Redirect chain | Rebind to the final URL; re-scope cookies per host |
| Slow single segment | Per-worker EWMA outlier | ETA-aware re-split (§4.3 of the engine spec) |
| Connection dies silently | No bytes within the idle window | Re-issue the outstanding range on a new worker |
| Server closes after N bytes | Repeated truncation at a similar offset | Reduce the requested range size to below N |

The last row is a good example of accumulated compatibility intelligence: it is cheap to
detect, invisible if you are not looking for it, and it makes the difference between "this
site does not work" and "this site works".

---

## 5. TLS

- `rustls` with the platform verifier, so the system trust store and enterprise roots work.
- TLS 1.3 preferred; TLS 1.2 permitted for compatibility. Nothing below TLS 1.2.
- Session resumption enabled — it is a meaningful saving when opening several connections to
  the same origin.
- ECH used opportunistically where the DNS record offers it.
- **No certificate-verification bypass, in any build, behind any flag.** A download manager
  that can be told to ignore certificates is a download manager that will be told to ignore
  certificates. If a corpus case needs a self-signed certificate, the test provides its CA to
  the trust store for that test only.

---

## 6. Proxies

| Type | Support | Notes |
| ---- | ------- | ----- |
| HTTP `CONNECT` | Yes | Baseline |
| HTTPS proxy | Yes | TLS to the proxy itself |
| SOCKS5 (+auth) | Yes | |
| PAC file | Stage 8 | Evaluate the script; cache results per host |
| System proxy detection | Yes | `libproxy`/env on Linux, WinHTTP on Windows |
| Basic / Digest auth | Yes | Credentials in the keyring |
| NTLM / Kerberos / Negotiate | Investigate in Stage 8 | The most likely reason to need the `curl` contingency backend |

Proxies frequently break range support, alter framing, and inject `Connection: close`. The
probe must be re-run when the effective proxy changes, and the result cached per
(origin, proxy) pair, not per origin alone.

---

## 7. What we deliberately do not do

| Not doing | Why |
| --------- | --- |
| Multipath QUIC | The IETF extension is still in draft and server support is effectively nil. Track it; do not build on it. |
| Multi-homed parallel connections (Wi-Fi + Ethernet) | Interesting and technically possible via interface binding. Post-1.0 — it changes the fairness story and needs its own testing. |
| Custom congestion control | We are a client. Use the stack's controller; do not attempt to out-compete other traffic on the user's own link. |
| HTTP/1.0 | Effectively extinct; ranges are unreliable. Single stream if encountered. |
| FTP / SFTP | Out of scope for 1.0 (`00-vision-and-scorecard.md` §5). |
| Faking `User-Agent` to defeat blocking | Boundary violation. We send an honest UA with an override the user sets consciously. |
