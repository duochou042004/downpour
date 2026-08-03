---
name: adr-new
description: Scaffold a new Architecture Decision Record for Downpour, with the options table, the decision, the consequences, and the reversal trigger. Use before writing code for any choice that is expensive to reverse — a library, a data model, an on-disk or wire format, a dependency in the transfer path, or non-trivial unsafe.
when_to_use: Choosing between two libraries or approaches; introducing or changing a persistent format or IPC message; adding a significant dependency; introducing unsafe; changing a stage boundary or a scorecard weight.
argument-hint: "[short title of the decision]"
allowed-tools: Read, Write, Edit, Glob, Bash(date:*)
---

# New Architecture Decision Record

An ADR records a decision that is **expensive to reverse**, with the trade-off that was
accepted and the evidence that would make us reverse it.

**Write it before the code.** An ADR written afterwards is a rationalisation — the point is to
make the trade-off visible while it is still cheap to choose differently.

## Procedure

### 1. Find the next number

```
ls docs/adr/
```

Take the highest and add one. Zero-padded to four digits. **Never reuse a number**, even for a
rejected ADR.

### 2. Confirm this actually warrants an ADR

It does if the change:

- picks between two libraries or two data models,
- introduces or changes a persistent on-disk format or an IPC message,
- adds a dependency in the transfer, storage, or IPC path,
- introduces `unsafe` in a non-trivial way,
- changes a stage boundary or a scorecard weight,
- relaxes anything in `docs/agent/INVARIANTS.md`.

It does not if the choice is cheap to reverse. Do not write an ADR for a function signature.
Over-documenting decisions is its own failure mode — it makes the ADR directory something
people stop reading.

### 3. Write it

`docs/adr/NNNN-kebab-case-title.md`:

```markdown
# ADR-NNNN: <the decision, stated as a decision>

- **Status:** proposed
- **Date:** <YYYY-MM-DD>
- **Stage:** S<n>
- **Deciders:** <who>

## Context

What forces this decision now? What constraints apply? What did we learn that made it
necessary? Someone reading this in two years should understand the situation without
reconstructing it.

## Options

| Option | Pros | Cons |
| ------ | ---- | ---- |

Include the option you rejected and **why**, fairly. An options table where every alternative
is obviously bad means you did not look hard enough — or you are writing a justification, not
a decision.

## Decision

What we are doing. One sentence, then the reasoning in order of weight.

## Consequences

**Easier:** …
**Harder:** …
**Accepted:** what we are knowingly giving up.

## Reversal trigger

**What evidence would make us change this?**

An ADR without this section is not finished — it is an opinion with a number on it. Be
specific: "if the corpus shows X by Stage 4", not "if it turns out badly".
```

### 4. Register it

Add a row to the index table in `docs/adr/README.md`.

Add an entry to `adr_index` in `state/progress.json`:

```json
{ "id": "ADR-0010", "title": "…", "status": "proposed", "superseded_by": null, "stage": "S2" }
```

Then run `node scripts/check-progress.mjs`.

### 5. Status

New ADRs are `proposed`. Only the maintainer moves one to `accepted`. Do not accept your own.

## Rules

- **ADRs are immutable once accepted.** To change a decision, write a new ADR that supersedes
  the old one and mark the old one `superseded by ADR-MMMM`. Never edit accepted history —
  the record of what we thought at the time is the value.
- `deferred` is a legitimate status. ADR-0002 defers the GUI toolkit on purpose, and records
  *why deferring is safe*, which is itself the decision.
- Write the honest trade-off. An ADR that makes the choice sound obvious is not useful to
  whoever has to revisit it.
