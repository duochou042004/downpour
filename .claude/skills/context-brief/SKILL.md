---
name: context-brief
description: Orient at the start of a Downpour session or task — read the progress file, identify the current stage and open work, and load only the two or three spec documents that the task actually needs. Use when starting a session, picking up a task, or when unsure which document applies.
when_to_use: Beginning of a session; handed a new task; unsure what to work on; unsure which spec covers the thing you are changing.
argument-hint: "[optional: the task you were given]"
allowed-tools: Read, Bash(node scripts/check-progress.mjs:*), Bash(bash scripts/doctor.sh:*), Glob, Grep
---

# Context brief

The ORIENT step of `docs/agent/HARNESS.md`, done properly.

`docs/` is large on purpose. Reading all of it for a small change burns the context you need
for the actual work. This skill loads exactly what the task requires.

## Procedure

### 1. Where are we?

```bash
node scripts/check-progress.mjs
```

Read `state/progress.json`:

- `current_stage` and its `name`
- Tasks with status `in_progress`, then `todo`, then `blocked`
- Unmet `exit_criteria`
- The last `session_log` entry, especially `left_in_progress`

### 2. Is the environment ready?

If the task involves building or testing:

```bash
bash scripts/doctor.sh
```

If required tools are missing, **say so and stop before attempting a build**. A failed `cargo`
invocation followed by three attempts to work around it wastes more than the check costs.

### 3. Which documents does this task need?

Open `docs/agent/CONTEXT-MAP.md` and find the row matching the task. Read only the documents
in the **Read** column — typically two or three.

If the task touches transfer, segmentation, or storage, also read
`docs/agent/INVARIANTS.md`. It is short and every line is load-bearing.

### 4. Does the task belong to this stage?

Check the task against the current stage's scope in `docs/12-roadmap-stages.md`.

**If it does not, stop and say so.** That is a scoping error by whoever assigned it, and it is
cheaper to fix in one sentence than in 400 lines of code that then has to be reverted or
carried as dead weight for four stages.

### 5. State the plan before touching anything

Two to five sentences (the SCOPE step of `HARNESS.md`):

- What behaviour will exist after this change that does not exist now.
- Which files you expect to create or modify.
- Which invariant this change could plausibly violate.
- What you are explicitly **not** doing.

If you cannot write that without hedging, you do not understand the task yet. Ask one precise
question rather than guessing confidently.

## Output

```markdown
**Stage:** S2 — Range, resume, validators, crash-safe storage
**Task:** S2-T7 — Journal replay with torn-tail handling
**Reading:** docs/04-storage-and-recovery-spec.md §3, docs/agent/INVARIANTS.md (I-9)
**Environment:** ready (cargo 1.97.1, nextest present)

**Plan:** Implement `Journal::replay` so it stops at the first record whose CRC fails and
truncates the file to the last valid record. Files: `crates/downpour-storage/src/journal.rs`,
`crates/downpour-storage/tests/journal_prop.rs`. Risk: I-9 — replay must never panic on
corrupt input, so the property test comes first. Not doing: compaction (S2-T8) or the SQLite
checkpoint reconciliation (S2-T9).
```

## Rules

- Do not read all of `docs/`. The context map exists so you do not have to.
- Do not start work before you can state the plan.
- Do not accept a task that belongs to a later stage, however easy it looks.
