# IPC and UI Specification

**Status: Normative.** IPC from Stage 1; UI in Stage 10.

---

## 1. The IPC contract is a public API

The CLI, the GUI, the native messaging host, and eventually third-party tools all speak it.
Treat it as public from the first commit — it is far cheaper to design it properly now than to
version it out of a mistake later.

**JSON-RPC 2.0**, framed, over:

| Platform | Transport | Path |
| -------- | --------- | ---- |
| Linux | Unix domain socket, mode `0600` | `$XDG_RUNTIME_DIR/downpour/daemon.sock` |
| Windows | Named pipe, DACL restricted to the user SID | `\\.\pipe\downpour-<user-sid>` |

Filesystem permissions are the first line of defence; a per-session token in the handshake is
the second. There is **no TCP listener**. Not on localhost, not behind a flag.

### 1.1 Handshake

```json
→ {"jsonrpc":"2.0","id":1,"method":"hello",
   "params":{"protocol_version":1,"client":"dp/0.1.0","token":"<from runtime dir>"}}

← {"jsonrpc":"2.0","id":1,
   "result":{"protocol_version":1,"daemon_version":"0.1.0","capabilities":["h2","media"]}}
```

Rules (I-11):

- The daemon **refuses** a client requesting a newer major `protocol_version` — it does not
  attempt to serve it.
- Additive changes only within a major version. New optional fields are fine; changed meanings
  are not.
- Every message carries `protocol_version`; a schema test round-trips every message type in
  CI.

---

## 2. Methods

### Downloads

| Method | Params | Returns |
| ------ | ------ | ------- |
| `download.add` | `AddRequest` (url, context, target, options) | `{ id }` |
| `download.get` | `{ id }` | `DownloadView` |
| `download.list` | `{ filter?, limit?, cursor? }` | `{ items, next_cursor }` |
| `download.pause` | `{ id }` | `{ state }` |
| `download.resume` | `{ id }` | `{ state }` |
| `download.cancel` | `{ id, delete_data: bool }` | `{ state }` |
| `download.remove` | `{ id, delete_data: bool }` | `{}` |
| `download.refresh` | `{ id, url, context? }` | `{ state, compatible: bool }` |
| `download.retarget` | `{ id, path }` | `{}` |
| `download.explain` | `{ id }` | `{ decisions: [ControllerDecision] }` |

### Queue

| Method | Params | Returns |
| ------ | ------ | ------- |
| `queue.get` | `{}` | `QueueState` |
| `queue.reorder` | `{ id, position }` | `{}` |
| `queue.set_limits` | `{ max_concurrent?, global_rate?, schedule? }` | `{}` |

### Media

| Method | Params | Returns |
| ------ | ------ | ------- |
| `media.inspect` | `{ url, context? }` | `{ variants: [MediaVariant] }` |
| `media.add` | `{ url, variant, target, context? }` | `{ id }` |

### System

| Method | Params | Returns |
| ------ | ------ | ------- |
| `system.status` | `{}` | `{ version, uptime, active, queued, throughput }` |
| `system.config.get` / `.set` | | |
| `system.shutdown` | `{ graceful: bool }` | `{}` |
| `system.repair` | `{ dry_run: bool }` | `{ findings }` |

### Events (server → client notifications)

```json
{"jsonrpc":"2.0","method":"event.progress",
 "params":{"id":"…","covered":1048576,"total":52428800,
           "rate":3145728,"workers":4,"eta_seconds":16}}
```

| Event | When |
| ----- | ---- |
| `event.progress` | Throttled, default 4 Hz per download |
| `event.state` | Any state transition |
| `event.error` | Recoverable and unrecoverable errors |
| `event.refresh_needed` | → `AwaitingRefresh`; the extension acts on this |
| `event.media_found` | Media detected |
| `event.recovery` | Daemon start recovery summary |

Clients subscribe with `subscribe`/`unsubscribe`. A slow client is dropped rather than allowed
to apply back-pressure to the engine — a hung GUI must never slow a transfer.

---

## 3. Error model

```json
{"jsonrpc":"2.0","id":7,"error":{
  "code": -32001,
  "message": "Representation changed on server",
  "data": {
    "kind": "validator_mismatch",
    "download_id": "…",
    "recoverable": true,
    "suggestion": "refresh_url",
    "detail": "ETag was \"abc\", now \"xyz\""
  }}}
```

`kind` is a stable machine-readable string. `message` is for humans and may be reworded;
`kind` may not. Clients switch on `kind`, never on `message`.

---

## 4. CLI (`dp`)

The CLI is the primary interface until Stage 10, and it stays a first-class interface after.
Anything the GUI can do, `dp` can do.

```bash
dp add <url> [--out PATH] [--connections N] [--rate LIMIT] [--referer URL] [--header K:V]
dp list [--state STATE] [--json]
dp status <id>
dp pause <id> | dp resume <id> | dp cancel <id> [--delete]
dp refresh <id> <new-url>
dp explain <id>              # why the controller chose what it chose
dp media inspect <url>       # list variants
dp media get <url> [--variant best|worst|1080p]
dp queue [--reorder id:pos] [--limit N] [--rate LIMIT]
dp repair [--dry-run]
dp daemon start | stop | status
dp config get|set <key> [value]
```

Conventions:

- `--json` on every read command, with a stable schema. The CLI is scriptable.
- Progress on a TTY, plain lines when piped.
- Exit codes: `0` success, `1` general error, `2` usage, `3` daemon unreachable,
  `4` download failed.
- No interactive prompt unless the terminal is interactive; `--yes` for automation.

---

## 5. Desktop UI

### 5.1 Why it is last

The daemon owns the state and the IPC contract is public. The GUI is therefore a *client*, and
the toolkit choice is a moderate-cost decision rather than a foundational one
(`docs/agent/WORKFLOW.md` §"reversibility ladder"). Deferring it to Stage 10 means we choose
with a year of ecosystem movement behind us instead of guessing now.

**Do not build UI before Stage 10.** See ADR-0002 for the current leaning and its open
questions.

### 5.2 Requirements when it arrives

| Requirement | Note |
| ----------- | ---- |
| Native rendering, no WebView | Avoids WebKitGTK/WebView2 as a runtime dependency |
| System tray with a context menu | Minimum: pause all, resume all, open, quit |
| Desktop notifications | Complete, failed, refresh needed |
| Idle CPU effectively zero | A download manager sits idle most of the time |
| Idle RSS under 60 MB | Scorecard G10 |
| Works on X11 and Wayland | Both, not "Wayland via XWayland" |
| Accessible | Keyboard navigation and screen-reader labels via AccessKit |
| Dark and light, following the system | Not a custom theme only |
| Localisable | Fluent or gettext; strings are never concatenated in code |
| Headless-testable | The UI must be testable in CI without a display server |

### 5.3 Screens

1. **Queue** — list, per-item progress, rate, ETA, worker count, state.
2. **Detail** — segment map visualisation, per-worker rates, the controller's decision log
   (`download.explain`), identity and URL history.
3. **Add** — URL, target, options, with a live probe result before the user commits.
4. **Media** — detected variants with a picker.
5. **Settings** — limits, schedule, capture rules, proxy, paths.

The segment map visualisation in (2) is not decoration. It is the fastest way for a user *or a
developer* to see that segmentation is behaving, and it makes an entire class of bug visible
at a glance.

---

## 6. Client behaviour rules

- Clients never write to the database or the journals. Only the daemon does.
- Clients never perform transfers. If a client can download, the architecture is defeated (I-12).
- Clients reconnect with backoff and resubscribe; a dropped connection is normal, not an error
  to show the user.
- Clients render the daemon's state; they do not maintain a competing model of it. On
  reconnect, they re-read rather than reconcile.
