# ADR-0005: HTTP backends behind a `TransferProtocol` trait

- **Status:** accepted
- **Date:** 2026-08-02
- **Stage:** S0

## Context

HTTP/3 is a genuine differentiator over IDM's protocol generation. It is also, in Rust as of
August 2026, explicitly experimental: `h3` is at 0.0.8 and documents that its API may change;
`reqwest`'s HTTP/3 support is an unstable feature; only the `quinn` QUIC backend is considered
production-usable.

Building the scheduler directly on any of those APIs means the scheduler churns whenever they do.

There is also a second, less obvious requirement: the deterministic simulation
(`09-testing-strategy.md` §4) needs to substitute the entire network layer.

## Options

| Option | Pros | Cons |
| ------ | ---- | ---- |
| **A `TransferProtocol` trait** | Backends swap freely; the simulator is just a backend; experimental code stays quarantined behind a feature flag | Abstraction cost; the trait must be designed well enough not to leak |
| Call `reqwest` directly | Simplest today | The scheduler churns with the library; simulation would need process-level network interception |
| Use `libcurl` for everything | Twenty-five years of compatibility knowledge, already written | A C dependency, cross-compilation pain, and a coarse-grained async model that fights Tokio |

## Decision

The engine never touches an HTTP library. It calls:

```rust
#[async_trait]
pub trait TransferProtocol: Send + Sync {
    async fn probe(&self, req: ProbeRequest) -> Result<RemoteObject, ProbeError>;
    async fn fetch_range(&self, req: RangeRequest, sink: RangeSink)
        -> Result<RangeOutcome, TransferError>;
    fn capabilities(&self) -> BackendCapabilities;
}
```

Backends: `h1h2` (production, S1), `h3` (feature-gated, off by default, S6), `sim`
(test-only), and `curl` as a **contingency** if the corpus proves a pure-Rust stack cannot
handle a class of proxy or authentication — most likely NTLM/Kerberos.

**Design rule:** if the scheduler ever needs to know which HTTP version it is talking to, that
knowledge flows through `BackendCapabilities`. Never a downcast, never a version check inside
the scheduler.

The `curl` contingency is deliberate. The goal is a 9.5/10 product, not a purity score. But it
is a contingency justified by corpus evidence, and taking it requires its own ADR.

## Consequences

**Easier:** experimental h3 cannot destabilise the scheduler; deterministic simulation becomes
possible at all; a stage-6 exit criterion — "disabling the h3 feature removes it entirely with
zero scheduler impact" — is a real, checkable proof that the abstraction holds.

**Harder:** the trait must express range semantics, redirects, connection reuse, and
capability differences without leaking protocol details. Getting it wrong shows up as
capability flags multiplying, which is the signal to redesign rather than to add another flag.

## Reversal trigger

If the trait accumulates more than a handful of capability flags whose only purpose is to let
the scheduler special-case a backend, the abstraction has failed. Redesign it — do not remove
it, because the simulation depends on it.
