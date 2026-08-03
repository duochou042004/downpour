# Tech Radar — August 2026

**Status: Living.** Verified 2026-08-02. Re-verify before each stage; version numbers rot.

This is the dependency baseline. Do not invent versions and do not add a crate that is not
here without checking it against §5 and recording it.

**Rust stable at time of writing: 1.97.1 (2026-07-14).** Edition 2024. MSRV is pinned in
`rust-toolchain.toml` and raised only with a reason.

---

## 1. Adopt — proven, use without further debate

| Crate | Version | Role | Note |
| ----- | ------- | ---- | ---- |
| `tokio` | 1.53 | Async runtime | ~90% of async Rust; epoll on Linux, IOCP on Windows. One runtime per process. |
| `hyper` | 1.11 | HTTP/1.1 + HTTP/2 core | The escape hatch under `reqwest` when we need connection-level control |
| `reqwest` | 0.13 | HTTP client | Stage 1–4 default; h3 remains an unstable feature |
| `rustls` | 0.23 | TLS | With the platform verifier, so enterprise roots work |
| `rusqlite` | 0.40 | SQLite | Direct, no ORM. WAL mode. |
| `serde` | 1.0 | Serialisation | |
| `thiserror` / `anyhow` | 2.0 / 1.0 | Errors | `thiserror` in libraries, `anyhow` only at binary boundaries |
| `tracing` | 0.1 | Structured logging | Spans per download and per segment |
| `clap` | 4.6 | CLI | Derive API; generates completions and man pages |
| `blake3` | 1.8 | Hashing | Fast enough to hash every block — this is what makes per-block verification affordable |
| `bytes` | 1.12 | Buffers | |
| `directories` | 6.0 | Platform paths | Never hard-code paths |
| `keyring` | 4.1 | Credential storage | Secret Service / kwallet / Credential Manager |
| `interprocess` | 2.4 | UDS + named pipes | One API for both platforms |
| `governor` | 0.10 | Rate limiting | Token bucket |
| `proptest` | 1.11 | Property testing | The interval map and journal depend on this |
| `criterion` | 0.8 | Benchmarks | |
| `insta` | 1.48 | Snapshot tests | Good for `explain` output and manifest parsing |
| `url` | 2.5 | URL parsing | Not a free choice: `reqwest`'s public API is already in terms of `url::Url`, so the alternative is converting at every boundary. Same project as `percent-encoding`. |
| `percent-encoding` | 2.3 | Percent-decoding | Needed for `Content-Disposition` `filename*` (RFC 8187) and URL path segments. Already in the tree via `url`, so it adds nothing transitively. |
| `mime` | 0.3 | Media types | Zero dependencies. Used for the "server sent an HTML error page" check in the probe. `RemoteObject.content_type` is typed as `Mime` in `03-transfer-engine-spec.md` §2.2. |
| `async-trait` | 0.1 | `async fn` in a `dyn` trait | Required, not chosen: `TransferProtocol` must be object-safe (ADR-0005 depends on swapping backends at runtime, and the simulator is one), and native `async fn` in traits is still not `dyn`-compatible. |
| `hyper-util` | 0.1 | Tokio glue for `hyper` 1.x | `hyper` 1.x deliberately ships without runtime glue; this is the official companion from the same project, and there is no way to run a `hyper` server or client on Tokio without it. |
| `http-body-util` | 0.1 | `Body` combinators | Same: `hyper` 1.x moved `Full`, `Empty` and `BodyExt` out of the core crate. |

Both `hyper-util` and `http-body-util` are currently used only by `tests/corpus` (the pathology
server and its reference client). `downpour-http` reaches `hyper` through `reqwest`.

## 2. Trial — use in a specific, bounded place

| Crate | Version | Role | Bounded how |
| ----- | ------- | ---- | ----------- |
| `quinn` | 0.11 | QUIC | Behind the `h3` feature flag. The only QUIC backend considered production-usable. |
| `h3` | 0.0.8 | HTTP/3 | **Explicitly experimental**; the API may change. S6 only, behind the trait, off by default. |
| `hickory-resolver` | 0.26 | DNS | For `HTTPS`/`SVCB` record lookup (RFC 9460). Not for general resolution. |
| `interavl` / `rangemap` | 0.6 / 1.7 | Interval tree | Evaluate both in S2; we may end up writing our own, since this structure is small and we property-test it exhaustively either way |
| `tray-icon` / `muda` | 0.24 / 0.19 | Tray + menus | S10. MIT/Apache, from the Tauri team, works with `winit`. |
| `accesskit` | 0.24 | Accessibility | S10 |
| `notify-rust` | 4.18 | Desktop notifications | Linux; Windows uses the platform API |
| `self_update` | 0.44 | Update check | AppImage and Windows only; never for distro packages |
| `cargo-dist` | 0.32 | Release matrix | S10 |
| `turmoil` | 0.7 | Network simulation | Evaluate for the simulation harness; we may need something more specific |

## 3. Assess — watch, do not build on

| Technology | Status August 2026 | Why it matters | Decision |
| ---------- | ------------------ | -------------- | -------- |
| **Multipath QUIC** | IETF draft (`draft-ietf-quic-multipath`), ~21 revisions, no meaningful server deployment | Would allow one connection over Wi-Fi + Ethernet simultaneously | Track. Do not build on a draft with no servers. |
| **`io_uring` runtimes** (`compio` 0.19, `monoio`, `tokio-uring` 0.5) | Real and improving; Tokio remains the production default. io_uring favours throughput over latency and most implementations are not work-stealing | Could remove the write path as a bottleneck at multi-gigabit | Post-1.0. A second I/O model doubles the crash-safety surface — the one place we cannot afford complexity. |
| **RFC 9530 `Repr-Digest`/`Content-Digest`** | Standard since 2024; CDN adoption still thin | Free server-supplied end-to-end integrity | **Use opportunistically now.** Costs nothing when absent, valuable when present. |
| **RFC 9460 `HTTPS`/`SVCB` DNS** | Standard; ~25% of top-1M domains publish; provider support broadening through 2026 | Learn `alpn=h3` before connecting — removes the `Alt-Svc` discovery round trip | **Adopt in S6.** Concrete advantage over IDM-generation clients. |
| **BBRv3** | `draft-ietf-ccwg-bbr` advancing; in Linux and several QUIC stacks | Better throughput on lossy paths | We are a client; use the stack's controller. Do not implement congestion control. |
| **Compression Dictionary Transport** | Shipping in Chromium | Irrelevant for large binaries | Ignore. |
| **`slint`** | 1.17.1, real desktop push through 2026 (tray, modals, shortcuts, rich text, drag&drop) | A strong GUI candidate | Assess for S10. Licence caveat — see §4. |
| **`iced`** | 0.14, reactive rendering, headless testing, hot reload; ships in COSMIC, Halloy, Sniffnet | The other strong GUI candidate | Assess for S10. MIT. See ADR-0002. |
| **`tauri`** | 2.11 | Mature, but pulls in WebKitGTK/WebView2 | Rejected for this project: a WebView runtime dependency for no engine benefit. |

## 4. Hold — deliberately not used

| Technology | Why not |
| ---------- | ------- |
| An ORM (`sqlx`, `diesel`, SeaORM) | The schema is tiny and the queries are hot. `rusqlite` directly. |
| A second async runtime | Deadlock source, no upside. |
| `openssl` | `rustls` avoids a C dependency and its cross-compilation pain. |
| WebView-based UI | A browser engine as a runtime dependency for a download manager is the wrong trade. |
| `unsafe` for performance | Not without a benchmark proving it matters and an ADR. |
| `curl` as the default backend | Contingency only, if the corpus proves pure Rust cannot handle a class of proxy/auth. |
| gRPC / protobuf for IPC | JSON-RPC is debuggable by hand and the extension speaks JSON natively. The message rate is trivial. |

### Licence caveat on Slint

`slint` is `GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0`.
The free path for an open-source desktop application is **GPL-3.0**, which would make the GUI
binary GPL-3.0 rather than Apache-2.0.

This is workable — the engine crates stay Apache-2.0 and only the GUI binary is GPL — but it
complicates the "one licence, reusable by anyone" story in ADR-0006. `iced` is MIT and has no
such issue. This is the central trade-off in ADR-0002 and it is a Stage 10 decision.

---

## 5. Rules for adding a dependency

Before adding a crate, answer these in the PR (or the ADR, if it is significant):

1. What does it do that we would otherwise write, and how much would that be?
2. Is it maintained? Last release, open issue count, bus factor.
3. What licence? Apache-2.0-compatible, or it does not go in.
4. How much does it pull in transitively? `cargo tree` before and after.
5. Does it contain `unsafe`? How much? (`cargo geiger`)
6. If it were abandoned tomorrow, what is the exit path?

**A crate that saves fifty lines and adds forty transitive dependencies is a bad trade.** A
crate that implements a protocol correctly and is used by thousands of projects is a good one.

Anything in the transfer path, the storage path, or the IPC path is a significant dependency
and gets an ADR.

---

## 6. Re-verification log

| Date | By | Changes |
| ---- | -- | ------- |
| 2026-08-02 | Initial research | Baseline established. Versions verified against crates.io. |
| 2026-08-03 | S1-T1 | Re-verified every §1 pin against crates.io before writing the workspace: `tokio` 1.53.1, `reqwest` 0.13.4, `rustls` 0.23.43, `thiserror` 2.0.19, `serde` 1.0.229, `proptest` 1.11.0, `clap` 4.6.5, `blake3` 1.8.5, `tracing` 0.1.44, `bytes` 1.12.1 — all consistent with the recorded majors, so nothing here was stale. Added `url`, `percent-encoding`, `mime` and `async-trait` to §1; none is a free choice (see the Note column), which is why they are recorded here rather than given ADRs of their own. |
