---
name: progress-update
description: Update state/progress.json after doing work on Downpour — set task statuses, record proof, append a session_log entry, and validate. Use at the end of any change that alters project status, and whenever the Stop hook asks for a progress update.
when_to_use: The user or a hook asks to update progress; you finished a task; you are ending a session; you found out-of-scope work that belongs in the backlog.
argument-hint: "[optional: what you did]"
allowed-tools: Read, Edit, Bash(node scripts/check-progress.mjs:*), Bash(date:*)
---

# Update the project progress record

`state/progress.json` is how this session hands off to the next one, and how a Claude Code
session hands off to a Codex session. Your context ends; this file does not.

**This is also readable as a plain procedure for agents without skill support (Codex). Follow
the steps by hand.**

## Procedure

### 1. Read the current state

```bash
node scripts/check-progress.mjs
```

Then read `state/progress.json`. Find the stage matching `current_stage`.

### 2. Update task statuses — honestly

For each task you touched:

| Set | When | Also required |
| --- | ---- | ------------- |
| `done` | The behaviour exists **and** a test proves it | `proof` names a real test that exists and passes |
| `in_progress` | Started, not finished | `notes` says exactly where you stopped and what the next concrete step is |
| `blocked` | Cannot proceed | `blocked_by` says what would unblock it |
| `todo` | Not started | — |
| `dropped` | Decided against | `notes` says why |

**`proof` is not optional for `done`.** "Implemented X" with `proof: null` is rejected by the
validator, and it should be — it is the exact failure mode the field exists to prevent. If you
have not run the test, the task is `in_progress`.

A good `notes` for `in_progress`:
> "Interval map split logic done and property-tested. Next: wire `SegmentAllocator::steal()`
> into the worker pool in `crates/downpour-engine/src/pool.rs`; the trait is defined but has
> no callers."

A useless one:
> "Working on segmentation."

### 3. Add new tasks if you decomposed work

New tasks use the stage's id prefix: `S1-T7`, `S2-T3`. Never reuse a number. Every new task
gets a `proof` field naming the test that will demonstrate it — a planned test name is fine at
creation time.

### 4. Update exit criteria only when they are actually met

`met: true` requires `evidence` that **someone else can reproduce**: a test name, a corpus case
id, a benchmark command. Not "I checked".

Partially met is **not met**. There is no amber.

Do **not** change `current_stage`. Stage advancement is a human decision — propose it with
`/stage-gate`.

### 5. Record out-of-scope findings in `backlog`

Anything you noticed but correctly did not do:

```json
{ "id": "B-3", "text": "Filename sanitisation should handle NFD/NFC normalisation on macOS",
  "found_in_stage": "S1", "target_stage": "S10", "rationale": "macOS is not a 1.0 target" }
```

This is what keeps scope discipline from becoming amnesia.

### 6. Append a session_log entry

Append-only. Never rewrite earlier entries to look tidier.

```json
{
  "at": "<ISO 8601 UTC, from `date -u +%Y-%m-%dT%H:%M:%SZ`>",
  "agent": "claude-code",
  "stage": "S1",
  "summary": "<what you actually completed, not what you attempted>",
  "tasks_touched": ["S1-T3", "S1-T4"],
  "files_touched": ["crates/downpour-http/src/probe.rs", "tests/corpus/ranges/"],
  "left_in_progress": "<or null>"
}
```

### 7. Update the header fields

- `updated_at` — now, ISO 8601 UTC. Must not be older than the newest `session_log` entry.
- `updated_by` — `claude-code`.

### 8. Update metrics if they changed

`corpus_cases_total`, `corpus_cases_passing`, `sim_seeds_fixed`, `invariants_covered`.

**`silent_corruption_findings` must stay 0.** If you found one, record it — and understand that
you have found a release blocker, not a bug to triage later (`docs/00-vision-and-scorecard.md`
§2).

### 9. Validate

```bash
node scripts/check-progress.mjs
```

Fix every error. Read every warning and decide about it consciously.

## Rules

- **Never mark something `done` you have not run.** Optimistic status is the fastest way to
  make this project unmanageable: the next session builds on a foundation that does not exist.
- The file is the source of truth. If a document disagrees with it, fix the document.
- If your change genuinely does not alter project status (fixing a typo in prose, say), still
  touch `updated_at` so the Stop hook sees an acknowledgement, and say so in one line.
