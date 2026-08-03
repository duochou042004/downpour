---
paths:
  - "extensions/**"
  - "crates/downpour-host/**"
---

# Browser extension and native host rules

This is the most exposed surface in the project. Everything the extension receives comes from
a web page; everything the native host receives comes from a browser process that any installed
extension can attempt to talk to.

## Treat every input as hostile

- Validate against the schema **before any field is used**, not while using it (I-13).
- Enforce the 1 MB frame cap on the length prefix *before allocating a buffer*. A 4 GB length
  prefix must be rejected without a 4 GB allocation.
- The native host makes **no network requests**, ever. If it can be made to fetch a URL, the
  browser sandbox has been usefully extended for an attacker.
- The native host logs to its own file. `stdout` is the protocol channel; writing to it corrupts
  the stream.
- Pin `allowed_origins` / `allowed_extensions` to our published extension IDs.

## Secrets

- Collect only cookies scoped to the specific request URL. Never the whole jar.
- Headers are **allow-listed**, not deny-listed. Forwarding everything the browser sends is how
  you leak something you did not intend to.
- Cookies and auth headers cross native messaging only, never a network socket.
- They reach the OS keyring, never SQLite, never a log at any level (I-14).

## Extension conduct

- No page modification beyond the download button, and that lives in a shadow root.
- No network requests of the extension's own. Native messaging is the only outbound channel.
- No analytics, no remote code, no remote configuration.
- Every requested permission needs a written justification for the store listing. Do not add a
  permission "in case we need it" — each one costs review time and user trust.

## Boundaries

- Streams protected by EME are **not offered**. Not offered-with-a-warning. Not offered.
- No TLS interception, no injected root CA, in any form.

## TypeScript

- `strict: true`. No `any`. No non-null assertions on values that came from outside.
- `biome` for lint and format.
- The Chromium and Firefox builds share a core with a thin adapter. Do not fork the extension.
- MV3 service workers terminate. Keep no in-memory state; reconstruct from `chrome.storage` on
  wake. Code that assumes the worker stayed alive works in testing and fails for users.
