---
name: corpus-case
description: Write a Downpour compatibility corpus case from a described server pathology — the YAML case, the pathology-server behaviour, and the assertions. Use whenever a bug is found, a stage needs its corpus category filled, or a server behaviour should become a permanent test.
when_to_use: A bug was found anywhere; filling a corpus category for a stage; a server behaviour needs to be locked in as a test; Gate D requires a regression case.
argument-hint: "[the pathology to capture]"
allowed-tools: Read, Write, Edit, Grep, Glob, Bash
---

# Write a compatibility corpus case

The corpus is Downpour's primary asset (ADR-0007). A case makes a bug permanently impossible
rather than temporarily fixed.

Read `docs/09-testing-strategy.md` §3 and `docs/01-idm-teardown.md` §3 first.

## Procedure

### 1. Check for an existing case

`ls tests/corpus/<category>/`. A duplicate case makes the suite slower without making it
stronger. If an existing case is close, extend it rather than adding a near-twin.

### 2. Name it after the pathology, not the fix

`etag-changed-midway`, not `fix-resume-bug-3`. The case outlives the bug, and in a year the
fix will be irrelevant while the pathology still exists.

### 3. Write it

```yaml
id: <kebab-case pathology name>
category: ranges | validators | framing | session | connections | redirects | proxies | protocols | local | media
description: >
  What the server does wrong, and why a reasonable engine might get it wrong.
  The second half matters — it is what tells a future reader why this case exists.
references: [RFC 9110 §14.4, INVARIANTS.md#i-3]

server:
  protocol: http/1.1 | http/2 | http/3
  content: { size: 50MB, pattern: deterministic-prng, seed: 42 }
  ranges: supported | absent | advertised-not-honoured
  etag: '"v1"'
  behaviour:
    - at: { bytes_served: 40% }
      then: { set_etag: '"v2"', change_content_from: 40% }

expect:
  final_state: completed | failed | awaiting_refresh | paused
  error_kind: <stable kind string, if failing>
  file_renamed: true | false
  silent_corruption: false        # on EVERY case, without exception
  bytes_spliced: 0
  max_connections: <if the case is about connection behaviour>
```

### 4. Content is always deterministic-from-seed

Never a stored fixture file. A seeded generator means the correct value of **any** byte is
computable, which turns corruption detection from "the hash differs" into "byte 4 194 305
should be 0x7A and is 0x00, so the range at 4 MiB was never written despite being marked
complete". That precision is the entire point.

### 5. Assert behaviour, not internals

"The engine stops with `validator_mismatch`" — good. "The engine calls `probe()` twice" — bad;
it breaks on the next refactor and tells you nothing about correctness.

### 6. If this case is for a bug fix

**Commit it failing first**, then the fix. That is Gate D in `docs/agent/HARNESS.md` and it is
not waivable. A fix without its case is a patch that the next refactor will quietly undo.

### 7. Register it

Add to the category index. Update `metrics.corpus_cases_total` in `state/progress.json`.

## Rules

- Never depend on a third-party server. Every case is local and offline.
- `silent_corruption: false` on every case. There is no case where corruption is acceptable.
- Prefer pathologies derived from a real RFC ambiguity, a real CDN behaviour, or a real bug
  report over invented ones.
