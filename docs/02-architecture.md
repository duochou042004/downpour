# Architecture

**Status: Normative.** Deviations require an ADR.

---

## 1. Shape

```text
┌──────────────────────┐   ┌──────────────────────┐   ┌─────────────────────┐
│ Chromium extension   │   │ Firefox extension    │   │  Desktop GUI        │
│ (MV3, TypeScript)    │   │ (WebExtensions, TS)  │   │  (Stage 10)         │
└──────────┬───────────┘   └──────────┬───────────┘   └──────────┬──────────┘
           │  native messaging (JSON over stdio)                 │
           └───────────────┬───────────────┘                     │
                           ▼                                     │
                 ┌───────────────────┐                           │
                 │  downpour-host    │  thin, stateless          │
                 │  (native msg host)│  translator + launcher    │
                 └─────────┬─────────┘                           │
                           │                                     │
        ┌──────────────────┴──────────────┬──────────────────────┘
        │  JSON-RPC 2.0 over UDS (Linux) / named pipe (Windows)
        │                                 │
        │                          ┌──────┴──────┐
        │                          │   dp (CLI)  │
        │                          └─────────────┘
        ▼
┌───────────────────────────────────────────────────────────────────────────┐
│                          downpourd  (the daemon)                          │
│                                                                            │
│  ┌────────────────┐  ┌──────────────────┐  ┌───────────────────────────┐  │
│  │ IPC Server     │  │ Queue Scheduler  │  │ Notification / Event Bus  │  │
│  └───────┬────────┘  └────────┬─────────┘  └───────────────────────────┘  │
│          │                    │                                            │
│  ┌───────┴────────────────────┴───────────────────────────────────────┐   │
│  │                       Transfer Engine                              │   │
│  │  Capability Probe → Protocol Router → Adaptive Concurrency Ctrl    │   │
│  │  Segment Allocator (interval map) → Worker Pool → Range Sink       │   │
│  └────────────────────────────┬───────────────────────────────────────┘   │
│                               │                                            │
│  ┌────────────────────────────┴───────────────────────────────────────┐   │
│  │                       Storage Layer                                │   │
│  │  Recovery Journal (append-only) → File Writer → sparse .dppart     │   │
│  │  SQLite (WAL): downloads, identities, block-map checkpoints        │   │
│  └────────────────────────────────────────────────────────────────────┘   │
│                                                                            │
│  ┌─────────────────────┐  ┌───────────────────┐  ┌──────────────────────┐ │
│  │ Credential Store    │  │ Compatibility     │  │ Metrics / tracing    │ │
│  │ (OS keyring)        │  │ Profiles          │  │                      │ │
│  └─────────────────────┘  └───────────────────┘  └──────────────────────┘ │
└───────────────────────────────────────────────────────────────────────────┘
```

---

## 2. The three architectural commitments

### 2.1 The daemon owns everything

`downpourd` holds all transfer state. Every other component — CLI, GUI, native host — is a
client that connects, subscribes, and issues commands. No client ever holds a transfer.

**Consequences that must hold (I-12):**

- Closing the GUI does not pause a download.
- Uninstalling the browser extension does not affect the queue.
- Two clients can be connected at once and see a consistent view.
- The daemon can be restarted and every download resumes from the journal.

This is the single most important structural decision, and it is why the UI toolkit choice is
a Stage 10 detail rather than a foundational one.

### 2.2 Everything crosses a versioned contract

The IPC surface is a versioned JSON-RPC 2.0 API (`08-ipc-and-ui-spec.md`). It is treated as a
public API from day one, because it is: the CLI, the GUI, the extension, and eventually
third-party tools all depend on it.

- Every message carries a `protocol_version`.
- The daemon refuses to serve a client requesting a newer major version (I-11).
- Schema changes are additive within a major version.

### 2.3 The protocol backend is behind a trait

The engine never calls an HTTP library directly. It calls:

```rust
#[async_trait]
pub trait TransferProtocol: Send + Sync {
    /// Discover what the remote will actually let us do. Never trusts advertisement.
    async fn probe(&self, req: ProbeRequest) -> Result<RemoteObject, ProbeError>;

    /// Fetch one byte range into the sink. Returns what was actually delivered.
    async fn fetch_range(
        &self,
        req: RangeRequest,
        sink: RangeSink,
    ) -> Result<RangeOutcome, TransferError>;

    /// Capabilities this backend can offer for this origin.
    fn capabilities(&self) -> BackendCapabilities;
}
```

This exists so that HTTP/3 — whose Rust ecosystem is still explicitly experimental
(`14-tech-radar-2026.md`) — can be added, swapped, or removed without touching the scheduler.
The same boundary makes the deterministic simulation possible: the simulator is just another
`TransferProtocol` implementation.

**Rule:** if the scheduler ever needs to know which HTTP version it is talking to, that
knowledge goes through `BackendCapabilities`, not through a downcast or a feature flag.

---

## 3. Crate layout

Workspace, landing incrementally from Stage 1. Crates are separated by *what invariant they
own*, not by layer-cake convention.

```text
crates/
├── downpour-types/        # shared types, IDs, errors, IPC message structs. No I/O.
├── downpour-intervals/    # the byte-interval map. Pure, exhaustively property-tested. No I/O.
├── downpour-storage/      # journal, file writer, SQLite. Owns durability (I-1, I-9, I-10).
├── downpour-http/         # TransferProtocol impls: h1/h2 backend, h3 backend, probe logic.
├── downpour-engine/       # scheduler, allocator, concurrency controller, worker pool.
├── downpour-ipc/          # JSON-RPC framing, transport (UDS / named pipe), auth.
├── downpour-daemon/       # downpourd binary: wiring, queue, lifecycle, signals.
├── downpour-cli/          # dp binary.
├── downpour-host/         # downpour-host binary: native messaging translator.
└── downpour-gui/          # Stage 10.

tests/
├── corpus/                # declarative compatibility cases + the pathology server
├── sim/                   # deterministic simulation harness
└── e2e/                   # daemon + client integration

extensions/
├── chromium/
└── firefox/
```

### Dependency direction

```
types ◀── intervals ◀── engine ──▶ http ──▶ types
              ▲            │
              │            ▼
              └───────── storage ──▶ types

daemon ──▶ engine, storage, ipc, http
cli, host, gui ──▶ ipc, types      (never engine, never storage)
```

**Hard rules:**

- `downpour-intervals` and `downpour-types` have **no I/O and no async**. They are pure, which
  is what makes them property-testable.
- `downpour-engine` does not know about SQLite or about `reqwest`/`hyper`. It knows traits.
- Clients (`cli`, `host`, `gui`) never link the engine. If a client can perform a transfer,
  the daemon architecture has been defeated.

---

## 4. Process model

| Process | Lifetime | Started by |
| ------- | -------- | ---------- |
| `downpourd` | Long-running; survives all clients | systemd user unit (Linux) / Windows service or autostart; also auto-started on demand by `dp` or `downpour-host` |
| `dp` | Per-invocation | User |
| `downpour` (GUI) | User session | User |
| `downpour-host` | Per browser session, per browser | The browser, via native messaging |

`downpour-host` is deliberately thin: it validates and translates messages between the
browser's stdio framing and the daemon's IPC, and it starts the daemon if it is not running.
It holds no state and makes no network requests. That keeps the browser-facing attack surface
small (I-13).

### Single-instance guarantee

The daemon uses an advisory lock on its runtime directory. A second instance detects the lock,
reports the existing socket path, and exits. Never two daemons over one database.

---

## 5. Concurrency model

- One Tokio multi-threaded runtime in `downpourd`. **No second runtime in the same process** —
  that is a class of deadlock we do not need.
- Each active download is one supervisor task owning N worker tasks.
- The supervisor owns the interval map; workers never mutate it directly. They request a
  grant and report an outcome. This is what makes I-2 structurally enforced rather than
  carefully maintained.
- File writes go through a per-download writer that owns the file handle. Positional writes
  (`pwrite`/`WriteFileEx` via `RandomAccess`-equivalent) mean workers do not contend on a
  seek cursor.
- Blocking work (`fsync`, SQLite) goes to `spawn_blocking` or a dedicated thread — never on
  the async executor.

**Back-pressure:** a worker that outruns the disk must slow down, not buffer unboundedly.
Channels between worker and writer are bounded, and the bound is a tuning knob with a
documented default.

---

## 6. Failure domains

Designed so that a failure in one place cannot take the system down.

| Failure | Blast radius | Behaviour |
| ------- | ------------ | --------- |
| One worker's connection dies | That range only | Range returns to the interval map, re-granted to another worker |
| One download hits an unrecoverable error | That download | Marked `error` with a reason; the queue continues |
| SQLite is corrupt or unreadable | Metadata | Journals are still authoritative; rebuild the database from journals |
| A journal's tail is torn | Last few seconds of that download | Replay to the last valid record; re-fetch the rest |
| The daemon is killed | All in-flight | Every download resumes from its journal on next start |
| A client misbehaves | That client | Its connection is dropped; transfers unaffected |
| The GUI crashes | UI only | Downloads continue; the GUI reconnects and resubscribes |

**Design rule:** the journal is the source of truth for progress; SQLite is a queryable
checkpoint. If they disagree, the journal wins. This is what makes "SQLite is corrupt" a
recoverable event rather than a data-loss event.

---

## 7. Configuration and state on disk

| Purpose | Linux | Windows |
| ------- | ----- | ------- |
| Config | `$XDG_CONFIG_HOME/downpour/config.toml` | `%APPDATA%\Downpour\config.toml` |
| State (DB, journals) | `$XDG_DATA_HOME/downpour/` | `%LOCALAPPDATA%\Downpour\` |
| Runtime (socket, lock) | `$XDG_RUNTIME_DIR/downpour/` | `\\.\pipe\downpour-<user-sid>` |
| Logs | `$XDG_STATE_HOME/downpour/logs/` | `%LOCALAPPDATA%\Downpour\logs\` |
| Credentials | OS keyring | Windows Credential Manager |

Resolved via the `directories` crate. Never hard-coded, never relative to the executable.

---

## 8. Observability

- `tracing` throughout, with structured fields. Spans per download and per segment.
- A `Secret<T>` newtype whose `Debug`/`Display` render `[redacted]`; cookies, auth headers,
  and signed-URL query strings use it (I-14).
- Per-download metrics: throughput EWMA per worker, RTT, retry counts, bytes re-fetched,
  concurrency decisions with their reasons.
- **The controller must be able to explain itself.** `dp explain <id>` prints why the engine
  chose the concurrency it chose. An adaptive system you cannot interrogate is an adaptive
  system you cannot debug, and this one will need debugging.

---

## 9. What is deliberately absent

| Not present | Why |
| ----------- | --- |
| A localhost HTTP server for browser integration | Unauthenticated local HTTP is a well-known hole. Native messaging is the correct channel. |
| A plugin system for the engine | Not before 1.0. It would freeze internals that are going to change. |
| An embedded browser or WebView | Adds WebKitGTK/WebView2 as a dependency for no engine benefit. |
| A second async runtime | Deadlock source with no upside. |
| An ORM | The schema is small and the queries are hot. `rusqlite` directly. |
| Dynamic linking of the engine into clients | Would break the daemon guarantee (I-12). |
