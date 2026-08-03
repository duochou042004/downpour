# ADR-0007: The compatibility corpus is the primary asset

- **Status:** accepted
- **Date:** 2026-08-02
- **Stage:** S0

## Context

The origin question was: *what is the secret technology that makes IDM good?*

The answer, from IDM's own published documentation, is that there is no secret. Its dynamic
segmentation is documented in a page of text and is a few hundred lines to reimplement. Its
actual advantage is roughly two decades of accumulated handling for servers that misbehave —
knowledge that lives in a very large set of specific cases, not in an algorithm.

That knowledge cannot be reproduced by writing more code, because the difficulty is not
writing the handler. It is *knowing the case exists*.

## Options

| Option | Assessment |
| ------ | ---------- |
| Reimplement IDM's algorithm and hope | This is what most alternatives did. It gets you 70% and a reputation for corrupting files on hard cases |
| Wait for user bug reports | This is how IDM did it. It takes twenty years and burns the users who hit the bugs |
| **Generate the pathology space deliberately** | Derive cases from the RFCs and from a taxonomy of what servers actually get wrong; enact them locally; make every case permanent |
| Test against real websites | Non-reproducible, non-deterministic, breaks when someone else's CDN changes, and cannot inject crashes |

## Decision

**The compatibility corpus in `tests/corpus/` is the project's primary asset.** The engine is
what runs against it.

Concretely:

1. A local pathology server serves **deterministic pseudo-random content from a seed**, so the
   correct value of any byte is computable without storing a large file. This is what makes
   corruption detection *exact* rather than checksum-shaped: not "the hash differs" but "byte
   4 194 305 should be 0x7A and is 0x00".
2. Cases are declarative YAML: the pathology, and the required engine behaviour.
3. The initial taxonomy (`09-testing-strategy.md` §3.2) targets ~159 cases across ten
   categories, derived from `01-idm-teardown.md` §3.
4. **Every bug found anywhere becomes a corpus case, in the same change that fixes it,
   committed failing first.** This is Gate D in the harness, and it is not waived.
5. No test depends on a third-party server, ever.

## Consequences

**Easier:** compatibility becomes measurable rather than anecdotal; regressions are structurally
prevented; the corpus is publishable and independently runnable, which turns "we are correct"
from a claim into something a reader can verify; contributors can add a case for a site that
fails them without understanding the engine.

**Harder:** the pathology server is real work before it pays off, and it must be built in
Stage 1 when there is nothing to test yet. Writing a case for every fix is a tax on every fix.

**Accepted:** that tax is the entire strategy. Skipping it once establishes that it is
optional, and then the corpus stops growing and we are back to hoping.

## Reversal trigger

None. If this decision is reversed, the project's answer to "how do you compete with twenty
years of accumulated knowledge" becomes "we hope", and the 9.5 target is not reachable.
