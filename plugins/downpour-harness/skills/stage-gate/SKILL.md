---
name: stage-gate
description: Produce the honest evidence table for a Downpour stage's exit criteria — what is met, what is not, and what evidence backs each claim. Use before proposing that a stage is complete, and as a mid-stage checkpoint to surface the remaining gaps by name.
when_to_use: Asked whether a stage is done; asked to close or advance a stage; a stage feels "nearly finished"; ~10 tasks have passed since the last checkpoint.
argument-hint: "[stage id, e.g. S2 — defaults to current_stage]"
allowed-tools: Read, Bash(node scripts/check-progress.mjs:*), Bash(cargo:*), Bash(just:*), Grep, Glob
---

# Stage gate review

A stage's exit gate is a wall (`docs/agent/WORKFLOW.md`). This skill produces the evidence
table that a human uses to decide whether to open it.

**You produce the table. You do not open the gate.** Agents propose; the maintainer disposes.
Never set `current_stage` yourself.

## Procedure

### 1. Load the stage

Read `state/progress.json`. Use `$ARGUMENTS` as the stage id if given, otherwise `current_stage`.

### 2. For every exit criterion, find the evidence — do not assume it

For each criterion, actually go and look:

- Does the named test exist? `Grep` for it.
- Does it pass? Run it if the toolchain is available.
- Does the corpus case exist and is it in the passing set?
- Is the benchmark result recorded somewhere reproducible?

Evidence is something **another person can re-run**. A test name, a corpus case id, a command.
"Implemented in `probe.rs`" is not evidence — it says the code exists, not that it works.

### 3. Check the task list

Any task not `done` or `dropped` blocks the gate. For each, say what remains.

### 4. Check the invariants

For each invariant in `docs/agent/INVARIANTS.md` that this stage touches, name the test that
covers it. An invariant with no test is an unmet criterion, whether or not it is written as one.

### 5. Run the gates

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo nextest run --workspace
just corpus      # if the stage has corpus cases
just sim         # if the stage has simulation scenarios
node scripts/check-progress.mjs
```

Report what actually happened, including failures, with their output.

### 6. Produce the table

```markdown
## Stage gate review — S2: Range, resume, validators, crash-safe storage

| Criterion | Status | Evidence |
| --------- | ------ | -------- |
| S2-C1 Crash injection at every write boundary → byte-correct | **met** | `just sim --scenario crash-matrix`, 10 000 seeds, 0 failures |
| S2-C2 Journal replay never panics under truncation/bit-flip | **met** | `crates/downpour-storage/tests/journal_prop.rs::replay_never_panics` |
| S2-C3 Interval map property tests over 10 000 sequences | **met** | `crates/downpour-intervals/tests/prop.rs::intervals_stay_disjoint_and_total` |
| S2-C4 Resume refuses on changed validator | **NOT MET** | Corpus case `etag-changed-midway` passes, but `weak-etag-only` is not written |
| S2-C5 disk-full-at-97pct resumable | **NOT MET** | Scenario exists; fails on seeds 7 and 41 — see output below |
| S2-C6 Rename only after verification | **met** | `tests/e2e/verify_before_rename.rs` |
| S2-C7 Zero silent corruption | **met** | 84/84 corpus cases, 0 findings |

**Open tasks:** S2-T9 (in progress), S2-T11 (blocked by S2-T9)

**Verdict: gate NOT ready.** Two criteria unmet, two tasks open.

**To close the gate:**
1. Write corpus case `weak-etag-only` (S2-C4)
2. Fix `disk-full-at-97pct` on seeds 7 and 41 (S2-C5) — output attached
3. Finish S2-T9, unblocking S2-T11
```

### 7. Update `state/progress.json`

Set `met` and `evidence` on criteria that genuinely qualify. Append a `session_log` entry
recording the gate review. Then run the validator.

## Rules

- **Report the failures.** An agent that softens a gate report to look productive has not saved
  anyone time — it has moved the discovery of the gap to a point where more work is built on
  top of it. That is the single most expensive mistake available in this project.
- Partially met is **not met**. There is no amber.
- "Works on my machine" is not evidence. A command someone else can run is.
- If a criterion turns out not to be objectively checkable, say so — that is a defect in the
  criterion and it needs rewriting before the gate can ever honestly close.
