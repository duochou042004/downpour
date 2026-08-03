# Workflow — the stage-gate process

`HARNESS.md` covers the loop for a single unit of work. This document covers the level above:
how a **stage** starts, runs, and closes, and who decides what.

---

## Why stages

Downpour is a system with ten interlocking parts, several of which can be silently wrong. The
failure mode for a project like this is not "we ran out of time" — it is "we built ten
features and then discovered the resume path corrupts files under load, and every one of the
ten has to be revisited".

Stages exist so that each layer is *proven* before the next layer depends on it. The rule is
blunt: **a stage's exit gate is a wall.** Work belonging to Stage N+2 is not written during
Stage N, even when it is one line and obviously fine.

---

## Roles

| Role | Who | Decides |
| ---- | --- | ------- |
| **Maintainer** | The human | Stage advancement, scope changes, ADR acceptance, releases |
| **Agent** | Claude Code / Codex | Implementation, tests, ADR drafts, evidence gathering |
| **Subagent** | Spawned by an agent | One narrow question or one narrow implementation, reported back |

Agents propose. The maintainer disposes. An agent never advances `current_stage`, never
accepts its own ADR, and never declares a release.

---

## Stage lifecycle

```
   planned ──▶ active ──▶ gate-review ──▶ complete
                  │            │
                  └──◀─────────┘   (gate fails: back to active with named gaps)
```

### Entering a stage

The maintainer sets `current_stage` in `state/progress.json`. On entry, an agent should:

1. Read the stage's `exit_criteria` and restate them in its own words. If any criterion is
   not objectively checkable, say so now — a vague exit criterion is a gate that can never
   honestly close.
2. Decompose the stage into `tasks` in `progress.json`. Each task needs an `id`, a `title`,
   and a `proof` field naming the test that will demonstrate it. `proof` may start as a
   planned test name, but a task cannot reach `done` with `proof: null`.
3. Identify the ADRs the stage will need and stub them as `proposed`.

### During a stage

Each task runs the `HARNESS.md` loop. In addition:

- **Weekly-equivalent checkpoint** (or every ~10 tasks, whichever comes first): run
  `/stage-gate` and read the honest gap list. This prevents the "90% done for three weeks"
  pattern by making the remaining 10% explicit and named.
- **New work discovered** goes to `backlog` with a stage tag. It does not get done now.
- **A blocked task** gets `status: "blocked"` and a `blocked_by` string. Blocked tasks are
  the maintainer's problem to unblock; keep working on unblocked ones.

### Closing a stage

Run `/stage-gate`. It produces an evidence table:

| Criterion | Status | Evidence |
| --------- | ------ | -------- |
| … | met / not met | test name, corpus case, benchmark run, or ADR |

Rules for the table:

- "Met" requires evidence that is **reproducible by someone else**. A test name, a corpus case
  id, a benchmark command. Not "I checked".
- Criteria that are partially met are **not met**. There is no amber.
- The agent presents the table honestly, including criteria it failed to satisfy. Softening
  the report to look productive is the worst thing an agent can do here, because it moves the
  discovery of the gap to a point where more work depends on it.

The maintainer reviews, then either sets the stage `complete` and advances `current_stage`, or
sends it back with named gaps.

---

## Cross-stage rules

### The reversibility ladder

Not all decisions are equally costly to undo. Prefer to defer the expensive ones.

| Cost to reverse | Examples | When to decide |
| --------------- | -------- | -------------- |
| Cheap | A helper function's signature, a log message | Whenever |
| Moderate | A crate choice inside one module, a CLI flag name | When first needed, ADR if non-obvious |
| Expensive | On-disk format, IPC schema, error model, async runtime | Early, with an ADR and a migration plan |
| Very expensive | Language, process model, licence | S0, ADR required, revisit only on strong evidence |

Corollary: the GUI toolkit is deliberately a **Stage 10** decision, because the daemon/IPC
architecture makes it moderate rather than expensive. Do not let it become an S0 argument.

### Spike work

Sometimes a stage needs a throwaway experiment — "does `quinn` actually give us per-stream
flow control the way we need?". That is fine, with three conditions:

1. It lives in `spikes/<name>/` and is never imported by production crates.
2. It has a `README.md` stating the question it answers and the answer it found.
3. It is deleted or promoted at the stage gate. A spike that survives two stage gates is
   either load-bearing (promote it properly) or dead (delete it).

### Regression policy

Any bug found at any time — during development, in CI, from a user — is fixed with:

1. A corpus case or simulation seed that reproduces it, **committed first and failing**.
2. The fix.
3. An `INVARIANTS.md` entry if it revealed a class of failure not already covered.

A fix without step 1 is not accepted. This is Gate D in `HARNESS.md`, and it is how the
compatibility corpus grows into the asset described in `09-testing-strategy.md`.

### Performance policy

Performance work is a stage activity, not a background one.

- Do not optimise before the correctness gate for that stage is green.
- Every optimisation needs a benchmark that shows the improvement and a test that shows the
  behaviour did not change.
- A performance regression against the recorded baseline is a bug of the same severity as a
  functional one, and blocks the gate.
- **Never** land an optimisation that weakens an invariant. If it is faster because it skips
  an `fsync`, it is not faster, it is broken.

---

## Handoff between sessions and agents

Your context ends. Write the handoff into `state/progress.json` before it does:

- Task statuses reflect reality, not intention.
- `session_log` gets an entry with what you actually completed and what you left mid-flight.
- Anything mid-flight gets `status: "in_progress"` with a `notes` field saying exactly where
  you stopped and what the next concrete step is.

A good handoff note: *"Interval map split logic done and property-tested. Next: wire
`SegmentAllocator::steal()` into the worker pool in `engine/src/pool.rs`; the trait is defined
but has no callers."*

A useless handoff note: *"Working on segmentation."*

---

## Release process

Not before Stage 10. When it applies, see `11-packaging-release.md`. The gate for a release
is the scorecard in `00-vision-and-scorecard.md`, not the roadmap being finished — the two
are different questions.
