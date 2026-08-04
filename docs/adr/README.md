# Architecture Decision Records

An ADR records a decision that is **expensive to reverse**, together with the trade-off that
was accepted and the evidence that would make us reverse it.

## When to write one

Write an ADR before the code when the change:

- picks between two libraries or two data models,
- introduces or changes a persistent on-disk format or an IPC message,
- adds a dependency in the transfer, storage, or IPC path,
- introduces `unsafe` in a non-trivial way,
- changes a stage boundary or a scorecard weight,
- relaxes anything in `docs/agent/INVARIANTS.md`.

An ADR written *after* the code is a rationalisation. The point is to make the trade-off
visible while it is still cheap to choose differently.

Use `/adr-new` to scaffold one.

## Template

```markdown
# ADR-NNNN: <short decision, stated as a decision>

- **Status:** proposed | accepted | deferred | superseded by ADR-MMMM
- **Date:** YYYY-MM-DD
- **Stage:** S0…S10
- **Deciders:** who

## Context
What forces this decision? What constraints apply? What did we learn that made this
necessary?

## Options
| Option | Pros | Cons |
| ------ | ---- | ---- |

## Decision
What we are doing. One sentence, then the reasoning.

## Consequences
What becomes easier. What becomes harder. What we are accepting.

## Reversal trigger
**What evidence would make us change this?** An ADR without this section is not finished —
it is an opinion with a number.
```

## Index

| ADR | Title | Status | Stage |
| --- | ----- | ------ | ----- |
| [0001](0001-language-rust.md) | Rust as the implementation language | accepted | S0 |
| [0002](0002-gui-toolkit-deferred.md) | GUI toolkit choice deferred to Stage 10 | accepted (deferred decision) | S0 |
| [0003](0003-daemon-first-architecture.md) | Daemon-first architecture with a public IPC contract | accepted | S0 |
| [0004](0004-storage-journal-plus-sqlite.md) | Append-only journal for progress, SQLite for metadata | accepted | S0 |
| [0005](0005-protocol-behind-a-trait.md) | HTTP backends behind a `TransferProtocol` trait | accepted | S0 |
| [0006](0006-license-apache-2.md) | Apache-2.0 licence | accepted | S0 |
| [0007](0007-corpus-as-primary-asset.md) | The compatibility corpus is the primary asset | accepted | S0 |
| [0008](0008-no-drm-no-mitm.md) | No DRM circumvention, no TLS interception | accepted | S0 |
| [0009](0009-adaptive-not-fixed-concurrency.md) | Adaptive concurrency instead of a fixed connection count | accepted | S0 |
| [0010](0010-corpus-case-format.md) | Declarative corpus cases over a frozen deterministic content generator | proposed | S1 |
| [0011](0011-ci-on-github-actions.md) | CI on GitHub Actions; GitLab kept as a mirror with CI disabled | proposed | S1 |
| [0012](0012-versioned-journal-binary-format.md) | Manually encoded, versioned binary recovery journal | proposed | S2 |
| [0013](0013-prefix-replay-and-verified-journal-compaction.md) | Fatal headers, prefix replay, and verified journal compaction | proposed | S2 |

## Rules

- ADRs are **immutable once accepted**. To change a decision, write a new ADR that supersedes
  the old one, and mark the old one `superseded by ADR-MMMM`. Never edit history.
- Number sequentially. Never reuse a number.
- `deferred` is a legitimate status. ADR-0002 is deferred on purpose: deferring is itself a
  decision, and recording *why* it can safely be deferred is valuable.
