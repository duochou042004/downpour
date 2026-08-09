# ADR-0020: Use bounded strict JSON-RPC frames over authenticated local sockets

- **Status:** proposed
- **Date:** 2026-08-10
- **Stage:** S3
- **Deciders:** maintainer (proposed by Codex)

## Context

ADR-0003 and `docs/08-ipc-and-ui-spec.md` fix the architectural contract: `downpourd` owns all
transfers, clients use JSON-RPC 2.0 over a mode-`0600` Unix-domain socket or a current-user-only
Windows named pipe, and the first request authenticates with a per-daemon-session token. They do
not fix the byte framing, the maximum daemon IPC frame size, token generation and provisioning, or
the representation used to make `protocol_version` mandatory on every typed message.

Those choices are public and expensive to reverse. They also sit on an untrusted-input boundary.
The decoder must reject an oversized frame before allocating its declared body, reject a malformed
or newer-version message before dispatch, and make it impossible for a failed handshake to invoke a
download handler. A local socket permission is necessary but not sufficient: another process under
the same account can reach it, so the second-factor token remains required by the normative spec.

The browser native-messaging channel already uses a four-byte little-endian length and a one-MiB
limit. Giving daemon IPC a different framing convention would add a translator and a second fuzz
surface for no benefit. JSON-RPC is already selected; hand-writing a JSON parser would add risk,
not control. The tech radar already adopts `interprocess` for the cross-platform local transport.

## Options

| Option | Pros | Cons |
| ------ | ---- | ---- |
| **Four-byte little-endian length plus strict JSON, capped at 1 MiB before allocation; `interprocess` local sockets; 256-bit OS-random session token** | Same framing as native messaging; constant-memory header validation; JSON-RPC remains inspectable; one Tokio transport API across Unix and Windows; token strength does not depend on clocks or process IDs | Frame boundaries are application-owned; strict DTO evolution needs care; Windows still needs an explicit user-SID DACL rather than library defaults |
| Newline-delimited JSON | Easy to inspect with shell tools | A cap requires scanning while buffering; pretty-printed or embedded newlines complicate framing; unlike native messaging |
| `Content-Length` text headers | Familiar from LSP and debuggable | More parser states and ambiguous malformed-header cases; no benefit on a local byte stream |
| JSON-RPC over localhost TCP | Libraries and test tooling are abundant | Explicitly forbidden by ADR-0003 and the security spec; widens the attack surface to the network stack |
| Platform-specific Tokio UDS plus direct Win32 named-pipe code | Minimal Unix dependency | Duplicates stream, accept, split and cancellation behavior; much more first-party unsafe Windows code |
| Trust socket/pipe permissions without a token | Less provisioning code | Contradicts the normative two-line defence and makes an accidentally broad ACL an authenticated command channel |

## Decision

**Frame every daemon IPC message as a four-byte unsigned little-endian payload length followed by
exactly that many UTF-8 JSON bytes, with `MAX_FRAME_BYTES = 1_048_576` enforced before allocation.**

The payload is JSON-RPC 2.0 and is decoded through private Serde DTOs with
`deny_unknown_fields`. `serde_json` is the only JSON implementation. The decoder first validates
the bounded frame, then the JSON-RPC envelope, then the method-specific DTO; only the resulting
typed command can cross into daemon dispatch. JSON `Value` is allowed inside the private envelope
only to select the method-specific decoder. It is never handed to a transfer or storage API.

The protocol version follows the existing hello example rather than inventing a JSON-RPC extension
member: request parameters and notification parameters carry `protocol_version`, successful result
objects carry it, and error `data` carries it. Every public message type therefore contains the
version in its typed payload. Version 1 accepts only version 1. A client requesting a higher major
receives a stable `protocol_version_unsupported` error and the connection is closed; ordinary
methods before a successful `hello` are rejected and never dispatched.

The S3 schema surface is deliberately limited to `hello`, `download.add`, `download.get`,
`download.resume`, and `system.status`. Later methods are additive version-1 DTOs. IDs and state
names use validated opaque strings on the wire so the IPC crate does not depend on storage; the
daemon owns conversion to its internal identifiers.

`interprocess` 2.4.3 with its Tokio feature provides the byte-stream local socket. Unix binds the
normative filesystem path with mode `0600` inside a mode-`0700` runtime directory. Windows binds a
local-only `\\.\pipe\downpour-<user-sid>-<runtime-id>` named pipe with an explicit DACL containing
the current user SID; the library or OS default descriptor is never accepted as proof. The runtime
ID is the first 128 bits of BLAKE3 over the canonical UTF-16 runtime-directory path. It preserves
single-daemon exclusion within one runtime root while allowing isolated roots to coexist in tests
and embedded use without a per-user global pipe-name collision. Platform-specific code may use
`interprocess`'s safe security-descriptor wrapper around a user-SID SDDL string. Any first-party
Windows FFI needed to obtain that SID stays in one module, has a `SAFETY` argument at each call, and
is covered by native Windows tests.

Each daemon start obtains 32 bytes from `getrandom` 0.4.3 and renders them as 64 lowercase hex
characters. The token type redacts `Debug` and `Display`. The daemon writes it with exclusive
creation and user-only permissions to the runtime token file before accepting clients; replacement
is by a new daemon session, never an in-place partial rewrite. Clients read it from that user-scoped
runtime location. Tokens, full signed URLs, and request context are never logged.

## Consequences

**Easier:** the daemon and native host share one framing shape; the maximum allocation is proven at
the four-byte boundary; every method has a compile-time schema; malformed input cannot partially
populate a command; generic in-memory streams exercise the same codec and handshake state machine
as UDS and named pipes; the transport dependency is isolated behind `downpour-ipc`.

**Harder:** every additive method needs request, result, and error DTOs plus round-trip and
newer-version tests. Token-file creation and endpoint ACLs need native Linux and Windows evidence.
The Windows current-user SID lookup is platform code and may require a small reviewed FFI boundary.
Strict unknown-field refusal means clients cannot send fields before the daemon version that
understands them; version-1 additions must therefore be coordinated by capability discovery or
optional fields in already-known DTOs.

**Accepted:** the one-MiB limit is shared with native messaging even though ordinary control
messages are far smaller. It leaves headroom for future batch/list responses while remaining a
hard pre-allocation ceiling. Large payloads do not belong on the control plane.

**Accepted:** authentication failure drops only that client. It never stops the listener, mutates a
download, or logs the supplied token. A client disconnect owns no transfer handle, so connection
cleanup cannot cancel daemon work.

## Reversal trigger

Replace `interprocess` if native Linux and Windows tests cannot prove the required `0600`/current-
user-DACL endpoint restrictions without bypassing its abstractions, or if its Tokio stream exhibits
a reproducible framing or cancellation defect. The replacement must retain byte-for-byte framing
and the same codec tests, so clients do not change with the transport library.

Raise `MAX_FRAME_BYTES` only when a named version-1 method has a legitimate response that cannot be
represented below one MiB and measurements show pagination is materially worse. Change the framing
only in a new protocol major after dual-stack interoperability tests; convenience alone is not a
reversal trigger.
