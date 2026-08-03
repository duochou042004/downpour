# ADR-0009: Adaptive concurrency instead of a fixed connection count

- **Status:** accepted
- **Date:** 2026-08-02
- **Stage:** S0, implemented in S4

## Context

Every download manager exposes a connection-count setting, and users have been trained to turn
it up. IDM defaults to 8 and allows 32. The implicit model is: more connections, more speed.

That model came from an HTTP/1.1 internet where a connection was the only unit of parallelism.
Three things changed:

1. **HTTP/2 and HTTP/3 multiplex streams over one connection.** Parallelism no longer requires
   a new handshake, so 16 connections is 16 handshakes for nothing.
2. **CDNs and origins cap aggressively.** Per-IP limits, per-account limits, `429`. Beyond the
   cap, more connections make the download *slower* and can get the user blocked.
3. **The optimum is situational.** Per-connection bandwidth caps reward many connections;
   per-IP caps punish them. The same number is right in one case and wrong in the other, and
   the user cannot know which they are facing.

## Options

| Option | Pros | Cons |
| ------ | ---- | ---- |
| Fixed count, user-configured | Simple, familiar, matches expectations | Wrong by default in most situations; users optimise by superstition |
| **Adaptive controller, user sets a ceiling** | Finds the optimum per server; degrades to 1 when that is best; cannot be tuned into self-harm | Must be explainable or it is undebuggable; a bad controller is worse than a fixed number |
| Machine-learned policy | Could capture per-CDN behaviour | No training data; unexplainable; over-engineering for 1.0 |

## Decision

An **Adaptive Concurrency Controller** (`03-transfer-engine-spec.md` §3):

- Start at 1 worker. Increase while aggregate throughput measurably improves.
- Back off on `429`, `503`, resets, or throughput regression.
- The user's setting is a **ceiling**, never a target.
- Add capacity protocol-appropriately: streams on h2/h3, connections on h1.1, escalating to a
  second connection only when evidence shows a per-connection bandwidth cap.
- EWMA-based and explainable. **No machine learning in 1.0.**

**Explainability is a hard requirement.** Every decision is recorded with its inputs and its
reason, surfaced by `dp explain <id>`. An adaptive system that cannot be interrogated cannot
be debugged, and this one will need debugging.

## Consequences

**Easier:** invariant I-7 becomes achievable; downloads stop getting rate-limited by their own
client; the engine is correct on both cap types without the user knowing which they face;
scorecard G6 ("never slower than a single connection") becomes reachable.

**Harder:** the controller is a feedback system and can oscillate, so it needs dedicated
simulation scenarios (`per-ip-cap`, `per-connection-cap`, `429-storm`, `throughput-plateau`).
Users conditioned by other tools will ask why "32 connections" does not appear to do anything;
the answer belongs in the documentation and in `dp explain`.

One specific trap: when a **user-set rate limit** is the binding constraint, the controller
must recognise it. Otherwise it reads its own throttle as a server plateau and adds workers
that cannot help. This needs its own test.

## Reversal trigger

If simulation shows the controller cannot beat a well-chosen fixed count across the scenario
set, simplify — a fixed count with per-origin learned defaults from `compat_profiles` would be
the fallback. Do not keep a complex controller that does not earn its complexity.
