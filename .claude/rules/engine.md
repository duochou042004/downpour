---
paths:
  - "crates/downpour-engine/**"
  - "crates/downpour-storage/**"
  - "crates/downpour-intervals/**"
  - "crates/downpour-http/**"
---

# Engine and storage rules

You are in the part of the codebase that can be **silently wrong**. Everything here exists to
uphold `docs/agent/INVARIANTS.md`. Read it before you change anything in these crates.

## Before you write

1. Which invariant does this change touch? Name it.
2. Which test layer proves it? (`docs/09-testing-strategy.md` — property / corpus / simulation)
3. Write that test first and watch it fail.

## The durability ordering is not negotiable

```
pwrite → fsync → journal append → journal fsync → mark Complete
```

If a change makes this faster by reordering, removing, or batching past a step, the change is
wrong. Batching *within* a step is fine and expected; moving the commit point is not. This
single ordering is invariant I-1, and reversing it is the corruption bug that has shipped in
most download managers at some point.

## The allocator is the only allocator

Workers never choose an offset. They receive a grant and write inside it. The writer rejects
an out-of-grant offset — panic in debug, hard error in release. That is invariant I-2 enforced
structurally rather than by discipline, and it must stay that way as work stealing gets more
sophisticated.

## Never trust the server

- `Accept-Ranges` is advertisement. Only an observed, validated `206` proves range support (I-6).
- `Content-Range` is validated against what was requested, never parsed and believed.
- `Content-Length` may lie in either direction. Handle both.
- A `200` where `206` was expected means the representation changed. Hard stop. Never write the
  body at an offset (I-3).
- Any `Content-Encoding` other than `identity` on a ranged response is rejected (I-5).
- A body that looks like HTML when a binary was expected is a session problem, not content.

## Never rename before verifying

The `.dppart` file becomes the real filename only after length, coverage, and digest checks
pass (I-4). There is no fast path around this.

## When you find a new failure mode

1. Add a corpus case or a simulation seed that reproduces it. Commit it failing.
2. Fix it.
3. If it revealed a class of failure not already covered, **add an invariant** to
   `docs/agent/INVARIANTS.md` in the same change.

Never weaken an invariant to make a test pass. If an invariant is genuinely wrong, that is an
ADR, not an edit.

## Performance

- Do not optimise before the stage's correctness gate is green.
- Every optimisation needs a benchmark showing the gain and a test showing behaviour is
  unchanged.
- An optimisation that weakens an invariant is not an optimisation. It is a regression with a
  better number attached.
