# ADR-0003: Daemon-first architecture with a public IPC contract

- **Status:** accepted
- **Date:** 2026-08-02
- **Stage:** S0

## Context

A download manager must keep downloading when its window is closed, must be reachable from a
browser extension, and must be scriptable. Those three requirements point at the same shape.

The alternative — an application that owns its transfers, with a headless mode bolted on —
is how several open-source download managers are built, and it is why closing their window
sometimes stops a download.

## Options

| Option | Pros | Cons |
| ------ | ---- | ---- |
| **Daemon + thin clients over IPC** | Downloads survive every client; one state owner; the GUI toolkit is swappable; scriptable by construction | More moving parts; IPC to design and version; daemon lifecycle per platform |
| GUI app with a background mode | Simpler to start | Closing the window is a transfer risk; the browser extension needs the app running; two state owners eventually |
| Library + several frontends | Flexible | Two frontends running means two engines means two writers on one file. Unacceptable given I-2 |

## Decision

`downpourd` owns all transfer state. The CLI (`dp`), the GUI, and the native messaging host
(`downpour-host`) are clients over **JSON-RPC 2.0** on a Unix domain socket (Linux) or a named
pipe (Windows).

The IPC surface is treated as a **public API from the first commit**, with a negotiated
`protocol_version` and additive-only changes within a major version.

No TCP listener. Not on localhost, not behind a flag: an unauthenticated local port is a
well-known hole, and native messaging removes the need for one.

## Consequences

**Easier:** invariant I-12 becomes structural rather than aspirational; the GUI decision
becomes deferrable (ADR-0002); third-party tooling gets a supported integration path;
scripting is free.

**Harder:** the daemon lifecycle differs per platform (systemd user unit vs. autostart);
version skew between client and daemon is now a real case that must be handled — the browser
extension will lag the daemon because store review takes weeks; every feature needs an IPC
surface designed, not just a function call.

**Accepted:** more upfront design in exchange for a system where "the window closed" is not a
category of bug.

## Reversal trigger

None foreseen. This is the load-bearing structural decision and reversing it would be a
rewrite. If IPC overhead ever measurably affects transfer throughput, that is a framing or
serialisation problem to fix, not a reason to collapse the processes.
