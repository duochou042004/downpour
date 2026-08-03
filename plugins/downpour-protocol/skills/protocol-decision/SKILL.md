---
name: protocol-decision
description: Decide the Downpour transfer strategy for an observed server behaviour — how many workers, whether to add streams or connections, and when to back off. Use when a server behaves unexpectedly, when tuning the concurrency controller, or when adding a row to the protocol decision table.
when_to_use: A download is slower than expected; a server caps or rate-limits; deciding streams vs connections; extending docs/05-protocol-matrix.md.
argument-hint: "[the observed behaviour]"
allowed-tools: Read, Grep, Glob, Edit, Bash
---

# Transfer strategy decision

Read `docs/05-protocol-matrix.md` §4 (the decision table) and ADR-0009 first.

## The core question

More parallelism helps when the bottleneck is **per-connection or per-stream**. It hurts when
the bottleneck is **per-IP, per-account, or the link itself**. The two look identical from the
outside until you measure.

Detection is always by observation, never by advertisement.

| If adding a worker… | The cap is | Strategy |
| ------------------- | ---------- | -------- |
| increases aggregate throughput roughly linearly | per-connection | Scale up until it stops |
| leaves aggregate throughput flat | per-IP, per-account, or the link | Settle at the minimum count that reaches the plateau |
| causes `429` / `503` / resets | server-enforced | Halve, honour `Retry-After`, cool down |
| increases throughput but the disk queue grows | the disk | Hold; adding network capacity the disk cannot absorb is waste |

## Protocol-aware capacity

Adding capacity does not always mean adding a connection.

| Negotiated | Add capacity by | Escalate to a new connection when |
| ---------- | --------------- | --------------------------------- |
| HTTP/1.1 | New connection | Always — it is the only option |
| HTTP/2 | New stream | Streams plateau **and** per-connection bandwidth capping is evident |
| HTTP/3 | New QUIC stream | Same test; also consider a second local interface if multi-homed |

Respect `SETTINGS_MAX_CONCURRENT_STREAMS`. Exceeding it gets streams refused, and a naive
engine misreads that as a transfer error and retries into the same wall.

## The trap to check for

When a **user-set rate limit** is the binding constraint, the controller must recognise it.
Otherwise it reads its own throttle as a server plateau and keeps adding workers that cannot
help. Every change to the controller should confirm this case still has a passing test.

## Before you finish

- [ ] The decision is driven by a measurement, not by a header
- [ ] Back-off is per-origin, not per-worker (five workers each retrying is a self-inflicted DoS)
- [ ] `dp explain` output makes this decision legible — inputs and reason, not just the outcome
- [ ] A simulation scenario covers the behaviour (`docs/09-testing-strategy.md` §4.2)
- [ ] If this is a new server behaviour, add a row to `docs/05-protocol-matrix.md` §4
