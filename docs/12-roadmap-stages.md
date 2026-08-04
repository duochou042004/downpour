# Roadmap — the ten stages

**Status: Living.** Update when reality diverges. `state/progress.json` is authoritative for
status; this document is authoritative for *content and exit criteria*.

---

## The principle

Ten stages, each ending in a wall. A stage's exit criteria must be **objectively checkable**
and must be met with **reproducible evidence** before the next stage begins
(`docs/agent/WORKFLOW.md`).

Why this order: every stage's output is a dependency of the next, and the stages that can be
*silently* wrong (S1–S4) come before the stages that are merely *visibly* wrong (S7–S10). We
prove the engine before we build anything on top of it.

The GUI is last on purpose. It is the most visible and least load-bearing part, and putting it
first is the standard way this kind of project dies with a beautiful window and an engine that
corrupts files.

---

## S0 — Foundations

*Status: in progress.*

Specification, decisions, and the harness. No product code.

**Deliverables**

- The `docs/` set (this document among them).
- ADRs 0001–0009 accepted.
- `state/progress.json` + schema + validator.
- Agent harness: `CLAUDE.md`, `AGENTS.md`, `docs/agent/*`, `.claude/`, `.codex/`.
- The in-repo plugin marketplace.
- `scripts/doctor.sh`.
- Repository skeleton, licence, `SECURITY.md`, `CONTRIBUTING.md`.
- CI skeleton: fmt, clippy, an empty test run that passes.

**Exit criteria**

1. Every ADR is `accepted` or explicitly `deferred` with a named trigger.
2. `node scripts/check-progress.mjs` passes against a populated `progress.json`.
3. `bash scripts/doctor.sh` runs clean on the development machine.
4. `CLAUDE.md` and `AGENTS.md` agree with each other and with `docs/`.
5. A fresh agent session can read `CLAUDE.md` → `HARNESS.md` → `progress.json` and correctly
   state what to work on next, without asking.

Criterion 5 is the real test of this stage. If a fresh session cannot orient itself, the
harness has failed regardless of how good the prose is.

---

## S1 — Robust single-stream downloader

The boring foundation, done properly.

**Deliverables**

- Cargo workspace; `downpour-types`, `downpour-http`, `downpour-cli`.
- `TransferProtocol` trait + the `h1h2` backend (`reqwest`/`hyper` + `rustls`).
- Capability probe (`03-transfer-engine-spec.md` §2).
- `dp add <url>` downloading a single file, single stream.
- Redirect chain capture, filename resolution and sanitisation.
- The pathology server with `ranges`, `framing`, and `redirects` categories.

**Exit criteria**

1. Downloads a 1 GB file correctly over HTTP/1.1 and HTTP/2.
2. The probe correctly classifies range support on all `ranges` corpus cases.
3. Filename sanitisation passes its property tests, including the traversal cases.
4. Never writes a body received with an unexpected `Content-Encoding` (I-5).
5. `redirects` and `framing` corpus categories pass.
6. Zero silent corruption across the S1 corpus subset.
7. A transient transport failure mid-body is retried and the download still completes
   byte-correctly; `Retry-After` is honoured exactly; back-off is per-origin (`03` §7).
8. The workspace compiles and its test suite passes on Windows.

Criteria 7 and 8 were added during Stage 1, on 2026-08-03, because the original six did not
test the two things the stage's own name promises. The stage is called **robust**, and nothing
in 1–6 exercises recovery from a transient failure — a single connection reset simply failed
the download. Downpour is also billed as Linux **and Windows**, and nothing had ever been
compiled for Windows at all, so the `cfg(windows)` branches behind I-10 had never seen a
compiler. A gate that a stage can pass while both of those are true is not a wall.

---

## S2 — Range, resume, validators, crash-safe storage

Where correctness is won or lost.

**Deliverables**

- `downpour-intervals` with full property tests.
- `downpour-storage`: journal, sparse preallocation, positional writes, durability ordering.
- Resume with `If-Range`; refuse on validator mismatch.
- SQLite schema + recovery on daemon start.
- The simulation harness and the `crash-matrix` scenario.
- `validators` and `local` corpus categories.

**Exit criteria**

1. Crash injection at **every** write boundary → byte-correct file after resume (I-1).
2. Journal replay never panics under `proptest` truncation and bit-flip (I-9).
3. Interval map property tests pass over 10 000 generated sequences (I-2).
4. Resume refuses on a changed strong validator; never splices (I-3).
5. `disk-full-at-97pct` leaves a resumable state (I-10).
6. Final rename happens only after full verification (I-4).
7. Zero silent corruption across the whole corpus so far.

**This is the highest-risk stage in the project.** Do not rush it. Everything after depends on
storage being trustworthy, and a defect here is invisible until it is catastrophic.

---

## S3 — Dynamic segmentation (HTTP/1.1)

**Deliverables**

- `downpour-engine`: segment allocator, worker pool, grant/split/complete.
- IDM-equivalent behaviour: split the largest remaining segment, reuse finished connections,
  respect `MIN_SPLIT_BYTES`.
- Per-worker throughput measurement.
- `connections` corpus category.

**Exit criteria**

1. N connections give measurably higher throughput on an uncapped server.
2. No overlapping writes under `worker-death` and `interleaving-fuzz` (I-2).
3. Connections are reused, not re-established, after a segment completes — verified by
   handshake count.
4. Falls back cleanly to a single stream when range support is absent (I-6).
5. Zero silent corruption.

---

## S4 — Adaptive concurrency and ETA-aware work stealing

Where we start to exceed IDM rather than match it.

**Deliverables**

- The Adaptive Concurrency Controller (`03-transfer-engine-spec.md` §3).
- ETA-aware splitting (§4.3).
- Tail handling, including optional redundant fetch.
- `download.explain` and `dp explain`.
- Rate limiting, with the controller aware of it.
- Simulation scenarios `slow-worker`, `per-ip-cap`, `per-connection-cap`, `429-storm`.

**Exit criteria**

1. The controller settles at or below the true optimum on every capped scenario (I-7).
2. Never slower than a single connection would have been (scorecard G6).
3. ETA splitting measurably beats naive halving on `slow-worker` — the tail time improves.
4. Back-off honours `Retry-After` exactly; no self-inflicted rate-limiting.
5. `dp explain` output is complete enough to diagnose a bad decision from the log alone.
6. Zero silent corruption.

---

## S5 — HTTP/2 stream parallelism

**Deliverables**

- Ranges as concurrent h2 streams on one connection.
- `SETTINGS_MAX_CONCURRENT_STREAMS` respected; flow-control windows tuned for bulk transfer.
- The protocol-aware capacity decision (streams vs. connections).
- `protocols` corpus category.

**Exit criteria**

1. Matches or beats the HTTP/1.1 path on an h2 origin, with one handshake instead of N.
2. Detects a per-connection bandwidth cap and correctly escalates to a second connection.
3. Never exceeds the advertised stream limit.
4. Flow-control tuning demonstrably improves throughput on a high-BDP path (documented
   before/after).

---

## S6 — HTTP/3 / QUIC (research backend)

**Deliverables**

- `h3` backend behind the trait, feature-gated, **off by default**.
- Protocol selection: DNS `HTTPS`/`SVCB` → h3 attempt → race against h2 → fallback.
- Per-stream and per-connection flow control tuned.
- 0-RTT on resume where safe.

**Exit criteria**

1. Downloads correctly over h3 against the corpus server.
2. Falls back within `H3_RACE_TIMEOUT` when UDP is blocked or throttled.
3. Beats h2 on the `slow-lossy` benchmark condition (no cross-stream head-of-line blocking).
4. **Disabling the feature flag removes h3 entirely with zero effect on the scheduler** — this
   is the proof the trait boundary is real.

The Rust h3 stack is explicitly experimental (`14-tech-radar-2026.md`). This stage may end
with "works, stays behind the flag", and that is an acceptable outcome.

---

## S7 — Browser extension and native messaging

The highest-value feature (`01-idm-teardown.md` §2.1).

**Deliverables**

- Chromium MV3 extension and Firefox extension from a shared core.
- `downpour-host` with schema validation and a fuzzed decoder.
- Download capture with full context: referer, cookies, headers, page URL.
- Capture rules with sensible defaults.
- Playwright end-to-end tests against the corpus server.

**Exit criteria**

1. Capture works on current Chrome, Edge, and Firefox (scorecard G8).
2. Downloads that fail with a bare URL succeed with captured context — demonstrated on a
   corpus case that requires cookies and a referer.
3. The host rejects every malformed frame in the fuzz corpus without a crash or an
   over-allocation (I-13).
4. Cookies never reach SQLite or any log (I-14).
5. Default capture rules do not break ordinary browsing — verified on a manual site list.

---

## S8 — Signed URLs, auth, proxies

**Deliverables**

- `AwaitingRefresh` + automatic refresh via the extension.
- Representation-compatibility check, including the sample-range comparison.
- Basic/Digest auth; proxy support (HTTP `CONNECT`, HTTPS, SOCKS5, PAC).
- Keyring-backed credential storage.
- `session` and `proxies` corpus categories.

**Exit criteria**

1. An expired signed URL is refreshed and the transfer continues from existing bytes (G4).
2. The compatibility check refuses an incompatible representation, every time (I-3).
3. Proxy cases pass, including proxies that break range support.
4. Credentials are in the keyring; the `trace`-level log grep test finds nothing (I-14).
5. NTLM/Kerberos investigated; either supported or a documented limitation with an ADR on the
   `curl` contingency.

---

## S9 — HLS / DASH

**Deliverables**

- Manifest parsers with fuzz targets.
- Variant selection; segment planning; ordered assembly.
- Optional FFmpeg remux.
- Segment-granularity resume.
- Rejection paths for DRM and live, with tests.

**Exit criteria**

1. Non-DRM HLS and DASH assemble byte-identically to a reference `ffmpeg` run (G9).
2. Every DRM and live case is *rejected*, with a test asserting the rejection.
3. Adversarial segment completion orders produce identical output.
4. Works without FFmpeg for single-container cases; reports clearly when FFmpeg is required.

---

## S10 — UI, queue, scheduler, packaging

**Deliverables**

- The GUI toolkit decision, finally made (ADR-0002 revisited with a year of evidence).
- The five screens (`08-ipc-and-ui-spec.md` §5.3), including the segment map.
- Tray icon, notifications, dark/light, accessibility, localisation scaffolding.
- Queue scheduler: priorities, concurrency limits, time-window scheduling.
- Packaging for all priority-1 targets; the update mechanism.

**Exit criteria**

1. Every scorecard row scored with evidence.
2. G10 resource targets met and measured.
3. Clean-machine install verified per target (G11).
4. Full release checklist (`11-packaging-release.md` §7) green.
5. Accessibility: full keyboard navigation and screen-reader labels on every screen.

---

## Deliberately after 1.0

Recorded so they are not smuggled in early:

FTP/SFTP · torrent/magnet · scheduling downloads for later start · a browser-independent
media extractor · mobile clients · macOS · socket activation · `io_uring` write path ·
multi-homed parallel connections · an engine plugin API · sync across machines.

Each needs an ADR arguing the scope change is worth the delay to the gates above.
