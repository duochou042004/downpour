---
name: invariant-check
description: Walk the Downpour engine invariants against a change before calling it done — for each of I-1 to I-14, state "not affected" or name the test that covers it. Use before proposing any change to the engine, storage, protocol, or IPC crates.
when_to_use: You changed anything under crates/downpour-engine, downpour-storage, downpour-intervals, downpour-http, or downpour-ipc; you are about to mark an engine task done; a review asks whether the invariants hold.
argument-hint: "[optional: the files or behaviour you changed]"
allowed-tools: Read, Grep, Glob, Bash(cargo:*), Bash(just:*)
---

# Invariant check

`docs/agent/INVARIANTS.md` lists fourteen properties. A violation of any of them is a release
blocker, not a bug. Three of them (I-1, I-2, I-3) describe failures that have shipped in most
open-source download managers at some point — including ones that had been stable for years.

This is Gate C from `docs/agent/HARNESS.md`. It is a checklist, not a formality.

## Procedure

### 1. Read `docs/agent/INVARIANTS.md`

Read it now, in full. It is short. Do not work from memory — the specific wording is what
matters, and it grows over time.

### 2. Establish what changed

Identify the files and the behaviour. If you cannot state in one sentence what behaviour is
different after this change, you are not ready to check invariants.

### 3. For each invariant, produce a verdict

| Verdict | Meaning | Required |
| ------- | ------- | -------- |
| **not affected** | This change cannot influence the invariant | One line saying why |
| **covered** | Affected, and a test proves it still holds | Name the test |
| **AT RISK** | Affected, and no test proves it | Stop. Write the test. |

"Probably fine" is `AT RISK`. So is "the existing tests still pass" when none of them exercise
the changed path.

### 4. Pay particular attention where this change lands

| If you touched… | Look hardest at |
| --------------- | --------------- |
| The write path or flush logic | **I-1** (durability ordering) — did the commit point move? |
| The allocator, splitting, or work stealing | **I-2** (disjoint intervals) — can two workers now overlap? |
| Resume, validators, or `If-Range` | **I-3** (never splice) — can two representations mix? |
| Completion or rename | **I-4** (verify before naming) |
| Request headers or response handling | **I-5** (encoding), **I-6** (proven ranges) |
| The concurrency controller | **I-7** (evidence-bounded concurrency) |
| Redirects, cookies, or URL handling | **I-8** (identity captured, not recomputed) |
| The journal format or replay | **I-9** (append-only, self-validating) |
| Preallocation or `ENOSPC` | **I-10** |
| Any persisted or wire format | **I-11** (versioned, refuses newer) |
| Client lifecycle or IPC | **I-12** (daemon outlives clients), **I-13** (validate at boundary) |
| Logging, cookies, credentials | **I-14** (secrets never on disk or in logs) |

### 5. Report

```markdown
## Invariant check — <what changed>

| Invariant | Verdict | Evidence |
| --------- | ------- | -------- |
| I-1 Durable before complete | covered | `sim::crash_matrix` seeds 0..10000 |
| I-2 No overlapping writes | covered | `intervals::prop::intervals_stay_disjoint_and_total` |
| I-3 Never splice | not affected | This change does not touch resume or validators |
| … | | |

**Result:** all invariants covered or not affected.
```

or

```markdown
**Result: I-7 is AT RISK.** The controller now increases concurrency on RTT improvement as
well as throughput gain, and no scenario asserts it still backs off on `429`. Writing
`sim::scenario::rtt_improves_but_429` before this goes further.
```

### 6. If you found a new failure mode

Add an invariant to `docs/agent/INVARIANTS.md` in the same change, with its mechanism and its
proof. The file is meant to grow — fourteen is where it started, not where it ends.

## Rules

- **Never weaken an invariant to make a test pass.** If an invariant is genuinely wrong, that
  is an ADR, not an edit.
- `AT RISK` blocks the change. Write the test.
- A performance improvement that weakens an invariant is a regression with a better number
  attached.
