# Testing Strategy

**Status: Normative.**

This is the most important document in the repository after `INVARIANTS.md`.

IDM's advantage is twenty years of accumulated knowledge about how servers misbehave
(`01-idm-teardown.md` §2.3). We cannot out-wait that. We can out-*generate* it: produce the
pathology space deliberately, in a lab, and turn every finding into a permanent, deterministic
test.

**The corpus is the product. The engine is what runs against it.**

---

## 1. Four layers

| Layer | Answers | Tool | Speed |
| ----- | ------- | ---- | ----: |
| **1. Property tests** | Do the data structures hold their invariants under arbitrary input? | `proptest` | ms |
| **2. Compatibility corpus** | Does the engine do the right thing against a misbehaving server? | Local pathology server | s |
| **3. Deterministic simulation** | Does the engine survive adversarial timing, ordering, and crashes? | Simulated clock + network | s–min |
| **4. Comparative benchmark** | Are we actually competitive? | Real servers + competitors | min |

A change is tested at the layer that matches its claim. Adding an integration test where a
property test belongs is how suites become slow and uninformative.

---

## 2. Layer 1 — Property tests

For pure data structures with algebraic invariants. Do not sample examples; assert the laws.

### The interval map (`downpour-intervals`)

```rust
proptest! {
    #[test]
    fn intervals_stay_disjoint_and_total(ops in arb_operation_sequence()) {
        let mut map = IntervalMap::new(TOTAL);
        for op in ops {
            map.apply(op);
            prop_assert!(map.all_disjoint());                 // I-2
            prop_assert_eq!(map.union_len(), TOTAL);
            prop_assert!(map.no_zero_length());
            prop_assert!(map.adjacent_same_state_merged());
        }
    }
}
```

Operations generated: grant, split at an arbitrary point, complete a sub-range, abandon a
grant, worker death, out-of-order completion, concurrent grants to the same region.

Additional properties:

- Completing all granted intervals yields exactly one `Complete` interval covering the file.
- Any interleaving of the same operation multiset yields the same final coverage.
- A split followed by completing both halves equals completing the whole.

### The journal (`downpour-storage`)

```rust
proptest! {
    #[test]
    fn replay_never_panics_and_yields_a_prefix(
        records in arb_records(),
        truncate_at in any::<usize>(),
        bitflip in arb_optional_bitflip(),
    ) {
        let bytes = corrupt(serialize(&records), truncate_at, bitflip);
        let state = Journal::replay(&bytes);            // must not panic  (I-9)
        prop_assert!(state.is_prefix_of(&records));
    }
}
```

### Others

Filename sanitisation (never escapes the target directory, always produces a valid name),
`Content-Range` parsing (never accepts a range inconsistent with what was requested), the
concurrency controller's state machine (never exceeds the ceiling, always able to reach 1).

---

## 3. Layer 2 — The compatibility corpus

### 3.1 What it is

`tests/corpus/` contains declarative YAML cases. Each names a server pathology and the
required engine behaviour. A local pathology server enacts the case; the engine runs against
it; assertions are checked.

```yaml
id: etag-changed-midway
category: validators
description: >
  The server's representation changes after 40% of the file has been fetched.
  The engine must stop rather than splice two versions together.
references: [RFC 9110 §13.1.3, INVARIANTS.md#i-3]

server:
  protocol: http/1.1
  content: { size: 50MB, pattern: deterministic-prng, seed: 42 }
  ranges: supported
  etag: '"v1"'
  behaviour:
    - at: { bytes_served: 40% }
      then: { set_etag: '"v2"', change_content_from: 40% }

expect:
  final_state: failed | awaiting_refresh
  error_kind: validator_mismatch
  file_renamed: false
  silent_corruption: false          # checked by full-content comparison
  bytes_spliced: 0
```

### 3.2 The taxonomy

Derived from `01-idm-teardown.md` §3. Every row there is at least one case here.

| Category | Cases (initial target) |
| -------- | ---------------------: |
| `ranges` — support, lies, malformed `Content-Range`, `200`-for-range, multipart | 25 |
| `validators` — missing, weak, changed, CDN-inconsistent | 15 |
| `framing` — no length, wrong length, chunked, encoding on range | 15 |
| `session` — signed URLs, referer, cookies, HTML-instead-of-file, `401`/`403` | 20 |
| `connections` — per-IP caps, per-connection caps, `429`, silent drops, close-after-N | 20 |
| `redirects` — chains, cross-host, loops, protocol downgrade | 10 |
| `proxies` — `CONNECT`, SOCKS5, auth, proxies that break ranges | 12 |
| `protocols` — h1/h2/h3 behaviour, ALPN mismatch, `Alt-Svc`, h3 fallback | 15 |
| `local` — disk full, permissions, path limits, collisions, network filesystems | 12 |
| `media` — manifest shapes, ordering, rejection of DRM and live | 15 |
| **Total (v1 target)** | **~159** |

### 3.3 The pathology server

A single Rust binary (`tests/corpus/server/`) that:

- speaks HTTP/1.1, HTTP/2, and HTTP/3 on demand;
- serves deterministic pseudo-random content from a seed, so any byte range's correct value is
  computable without storing a large file — **this is what makes corruption detection exact**;
- enacts any behaviour a case describes: delays, caps, resets, wrong headers, mid-transfer
  changes, expiring tokens;
- records everything it received, so a case can assert on request shape as well as on outcome.

Deterministic content is the keystone. Corruption detection is not "the checksum differs" — it
is "byte 4 194 305 should be `0x7A` and is `0x00`, which means the range starting at 4 MiB was
never written despite being marked complete."

### 3.4 The growth rule

**Every bug found anywhere becomes a corpus case, in the same change that fixes it, committed
failing first.** (Gate D in `docs/agent/HARNESS.md`.)

This is the mechanism by which we accumulate in months what IDM accumulated in decades. The
corpus only works if the rule is followed without exception — a fix without a case is a fix
that will be undone.

---

## 4. Layer 3 — Deterministic simulation

The corpus tests *what* the engine does. Simulation tests what happens when timing is
adversarial — the failures that appear once a week in production and never in CI.

### 4.1 Principle

Replace the clock and the network with controllable implementations. Everything derives from a
seed. **A failing seed reproduces exactly, on any machine, forever.**

This is possible because of the `TransferProtocol` boundary (`02-architecture.md` §2.3): the
simulator is just another backend.

```rust
#[test]
fn crash_at_every_write_boundary() {
    for seed in 0..10_000 {
        let sim = Simulation::new(seed)
            .with_content(Content::deterministic(500 * MB))
            .with_network(Network::lossy(0.02).rtt_jitter(10..200))
            .with_crash_policy(CrashPolicy::AtEveryWriteBoundary);

        let outcome = sim.run_download_to_completion_with_restarts();

        assert!(outcome.file_matches_expected_content());   // I-1
        assert_eq!(outcome.silent_corruptions, 0);
        assert!(outcome.bytes_refetched < 0.05 * outcome.total);
    }
}
```

### 4.2 Scenario set

| Scenario | Asserts |
| -------- | ------- |
| `crash-matrix` | Crash at every write boundary → byte-correct after resume (I-1) |
| `kill-9-storm` | Repeated hard kills at random points → always resumable |
| `power-loss` | fsync-boundary loss → prefix-consistent journal (I-9) |
| `slow-worker` | One worker at 5% of the others → ETA rebalancing triggers, tail is not dominated |
| `worker-death` | Workers die at random → ranges are reclaimed, no overlap (I-2) |
| `per-ip-cap` | Controller settles at the cap, does not oscillate (I-7) |
| `per-connection-cap` | Controller scales up to the throughput plateau |
| `429-storm` | Back-off honoured; no self-inflicted DoS |
| `disk-full-at-97pct` | Clean pause, resumable, no truncation (I-10) |
| `network-flap` | Repeated loss/restore → progress, no corruption |
| `url-expiry-race` | URL expires during a segment fetch → correct refresh, no splice |
| `clock-jump` | System clock jumps forward and backward → no ETA or timeout pathology |
| `interleaving-fuzz` | Randomised task scheduling → coverage is order-independent |

### 4.3 Continuous seed exploration

CI runs a fixed seed set on every commit and a large random set nightly. Any seed that fails
is **added to the fixed set permanently**, with the bug it found named in a comment. The fixed
set becomes a growing record of every timing bug we have ever had.

---

## 5. Layer 4 — Comparative benchmark

We claim 9.5/10 against IDM. That claim needs measurement, and losing a case is fine as long
as we know we lost it.

### 5.1 Method

Same file, same server, same network conditions (shaped with `tc netem`), run against:

- Downpour
- IDM (Windows only)
- AB Download Manager
- aria2c
- Browser-native download (Chrome, Firefox)

### 5.2 Metrics

| Metric | Why |
| ------ | --- |
| Time to first byte | Probe overhead |
| Time to 90% of link capacity | How fast the controller finds the right concurrency |
| Mean throughput | The headline number |
| Tail time (last 5%) | Where naive schedulers lose — this is where ETA splitting should show |
| Handshake count | Efficiency; matters on high-RTT links |
| Bytes re-fetched | Waste from retries and redundant fetch |
| Peak RSS / mean CPU | Scorecard row 6 |
| Resume success rate | Correctness under interruption |
| **Silent corruption count** | **Must be zero. Non-negotiable.** |

### 5.3 Conditions

| Condition | Shape |
| --------- | ----- |
| Fast, low latency | 1 Gbps, 5 ms |
| Fast, high latency | 1 Gbps, 200 ms (intercontinental) |
| Slow, lossy | 10 Mbps, 100 ms, 2% loss |
| Mobile-like | 50 Mbps, 60 ms, variable |
| Per-connection capped | 5 Mbps per connection |
| Per-IP capped | 50 Mbps total |
| CDN-like | Multiple edges, varying rates |

Results are published in release notes with the raw data. A benchmark you cannot reproduce is
marketing, not engineering.

---

## 6. What CI runs

| Trigger | Suite | Budget |
| ------- | ----- | -----: |
| Every push | fmt, clippy `-D warnings`, unit, property (fixed seeds), corpus (fast subset) | < 5 min |
| Every PR | + full corpus, simulation (fixed seed set), `cargo deny`, `cargo audit` | < 20 min |
| Nightly | + simulation with 10 000 random seeds, fuzz targets, cross-platform matrix | hours |
| Pre-release | + full benchmark suite, manual browser matrix, clean-machine install | — |

Fuzz targets: IPC decoder, native-messaging frame decoder, journal replay, HLS/DASH manifest
parsers, `Content-Range` and `Content-Disposition` parsers. All are attack surfaces reachable
from untrusted input.

---

## 7. Rules

1. **No test depends on a third-party server.** Every case is local and offline. A test that
   fails because someone else's CDN changed is a test that will be disabled and then deleted.
2. **No flaky tests.** A test that fails intermittently is a bug in the test or in the code —
   never a "known flake". Deterministic simulation exists precisely so that "intermittent" is
   not a category we accept.
3. **Tests assert behaviour, not implementation.** A test that breaks on a refactor with no
   behaviour change was testing the wrong thing.
4. **Every fix ships with its regression case.** Gate D.
5. **Corruption tests compare content, not checksums of our own making.** The deterministic
   generator gives us ground truth for every byte; use it.
6. **Coverage is a diagnostic, not a target.** 100% coverage of code that asserts nothing is
   worthless. Invariant coverage is what matters, and it is tracked by hand in
   `INVARIANTS.md`.
