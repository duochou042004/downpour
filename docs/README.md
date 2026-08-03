# Downpour documentation

Numbered documents are the specification. `agent/` is the operating harness for AI coding
agents. `adr/` is the decision record. `reference/` is source material.

**Do not read all of this for one task.** Use [`agent/CONTEXT-MAP.md`](agent/CONTEXT-MAP.md) to
find the two or three documents your task actually needs.

## Start here

| If you are… | Read |
| ----------- | ---- |
| An AI agent starting a session | [`agent/HARNESS.md`](agent/HARNESS.md) → [`../state/progress.json`](../state/progress.json) → [`agent/CONTEXT-MAP.md`](agent/CONTEXT-MAP.md) |
| A human new to the project | [`00-vision-and-scorecard.md`](00-vision-and-scorecard.md) → [`02-architecture.md`](02-architecture.md) → [`12-roadmap-stages.md`](12-roadmap-stages.md) |
| Wondering why a choice was made | [`adr/README.md`](adr/README.md) |
| About to touch the engine | [`agent/INVARIANTS.md`](agent/INVARIANTS.md) |

## The specification

| # | Document | What it defines | Status |
| - | -------- | --------------- | ------ |
| 00 | [Vision and scorecard](00-vision-and-scorecard.md) | What "9.5 out of 10" means, as a weighted measurable table | Normative |
| 01 | [IDM teardown](01-idm-teardown.md) | What IDM actually does, black-box; the taxonomy of what goes wrong | Informative |
| 02 | [Architecture](02-architecture.md) | Process model, crate layout, concurrency, failure domains | Normative |
| 03 | [Transfer engine](03-transfer-engine-spec.md) | Probe, adaptive concurrency, segment allocation, URL refresh | Normative |
| 04 | [Storage and recovery](04-storage-and-recovery-spec.md) | Journal, durability ordering, SQLite, verification, filenames | Normative |
| 05 | [Protocol matrix](05-protocol-matrix.md) | Per-protocol strategy; the server-behaviour decision table | Normative |
| 06 | [Browser integration](06-browser-integration-spec.md) | Extension, native messaging, context capture, media detection | Normative |
| 07 | [Media](07-media-spec.md) | HLS and DASH, non-DRM only | Normative |
| 08 | [IPC and UI](08-ipc-and-ui-spec.md) | The JSON-RPC contract, the CLI, the GUI requirements | Normative |
| 09 | [Testing strategy](09-testing-strategy.md) | Property tests, the compatibility corpus, simulation, benchmarks | Normative |
| 10 | [Security, privacy, legal](10-security-privacy-legal.md) | Threat model, secret handling, and the absolute boundaries | Normative |
| 11 | [Packaging and release](11-packaging-release.md) | Targets, pipeline, updates, the release checklist | Normative |
| 12 | [Roadmap](12-roadmap-stages.md) | The ten stages and their exit criteria | Living |
| 13 | [Glossary](13-glossary.md) | Shared vocabulary | Informative |
| 14 | [Tech radar](14-tech-radar-2026.md) | The dependency baseline, verified 2026-08-02 | Living |

## The agent harness

| Document | Purpose |
| -------- | ------- |
| [`agent/HARNESS.md`](agent/HARNESS.md) | The operating manual: the loop, the gates, the memory, the boundaries |
| [`agent/INVARIANTS.md`](agent/INVARIANTS.md) | The fourteen properties that must always hold. Violations block releases |
| [`agent/WORKFLOW.md`](agent/WORKFLOW.md) | The stage-gate process, roles, reversibility, handoff |
| [`agent/DEFINITION-OF-DONE.md`](agent/DEFINITION-OF-DONE.md) | Three levels: task, stage, release |
| [`agent/CONTEXT-MAP.md`](agent/CONTEXT-MAP.md) | Which document to read for which task |
| [`agent/CODEX-NOTES.md`](agent/CODEX-NOTES.md) | Codex-specific mechanics and what it does not get |

## Conventions

- **Normative** — the implementation must match. Deviating requires an ADR.
- **Informative** — background and rationale. Useful, not a contract.
- **Living** — expected to change. Update it when reality diverges.
- Invariants are referenced as `I-1` … `I-14`, defined in [`agent/INVARIANTS.md`](agent/INVARIANTS.md).
- Stages are `S0` … `S10`, defined in [`12-roadmap-stages.md`](12-roadmap-stages.md).
- Decisions are `ADR-NNNN`, indexed in [`adr/README.md`](adr/README.md).
- Scorecard gates are `G1` … `G11`, defined in [`00-vision-and-scorecard.md`](00-vision-and-scorecard.md).
