# ADR-0008: No DRM circumvention, no TLS interception

- **Status:** accepted
- **Date:** 2026-08-02
- **Stage:** S0

## Context

Two capabilities are technically achievable and would expand what Downpour can capture:

1. Decrypting protected streams (Widevine, PlayReady, or the HLS AES-128 middle ground).
2. A local TLS man-in-the-middle proxy to observe all browser traffic, as some download
   managers have historically used.

Both would be asked for. Recording the decision now means the answer does not get relitigated
every time someone files the issue.

## Decision

Neither is implemented. Ever. These are boundaries, not priorities.

### No DRM circumvention

- Encrypted HLS and DASH are **not offered**, not "offered with a warning".
- No licence-server interaction, no key extraction, no CDM emulation.
- The rejection paths have tests; a regression that starts accepting an encrypted stream
  fails CI (`07-media-spec.md` §7).

**On HLS AES-128 specifically:** it is often only a deterrent rather than a licensing system,
and the argument that "the user's session already has the key" is not unreasonable. It is
still an access-control measure, and the engine cannot reliably distinguish a legitimately
supplied key from one obtained in a way the site did not intend. Not in 1.0. Revisiting
requires an ADR containing an actual legal analysis, not a code change.

### No TLS interception

- No MITM proxy, no injected root CA, not as a default, not opt-in, not behind a developer flag.
- No certificate-verification bypass in any build.

Installing a root CA on a user's machine is a permanent, systemic weakening of their security
in exchange for a feature. Native messaging achieves the same integration goal without it.

## Consequences

**Easier:** Downpour stays packageable in distribution repositories and listable in browser
extension stores — one DRM feature makes it a tool distributions will not carry. The security
review surface stays small. The project does not need a legal position per jurisdiction.

**Harder:** we will lose feature comparisons against tools that do these things, and there
will be recurring issue reports asking for them. The README states the limitation plainly so
that the expectation is set before installation rather than after.

**Accepted:** a smaller feature set in exchange for a tool that can be recommended, packaged,
and trusted.

## Reversal trigger

None for DRM and TLS interception. The AES-128 HLS sub-case may be revisited **only** via a
new ADR containing a jurisdiction-aware legal analysis — and even then, it would ship as an
explicitly opt-in, clearly labelled capability, or not at all.
