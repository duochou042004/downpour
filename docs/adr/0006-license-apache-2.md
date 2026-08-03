# ADR-0006: Apache-2.0 licence

- **Status:** accepted
- **Date:** 2026-08-02
- **Stage:** S0

## Context

The project is explicitly "completely open source" and intended to serve the community. The
licence choice determines who can use the engine, and it constrains which dependencies can be
linked.

## Options

| Option | Pros | Cons |
| ------ | ---- | ---- |
| **Apache-2.0** | Permissive; an **explicit patent grant**; corporate-friendly; matches AB Download Manager; compatible with GPL-3.0 downstream | Does not require downstream to share improvements |
| MIT | Simplest, most permissive | No patent grant — a real gap for a project implementing network protocols |
| MIT OR Apache-2.0 | Rust ecosystem convention; maximum compatibility | Two files, slightly more explanation, and no practical gain here |
| GPL-3.0 | Improvements come back; matches aria2, gopeed, XDM | Blocks reuse of the engine by permissive projects; and the engine being reusable is a stated goal |
| MPL-2.0 | File-level copyleft | Less familiar; complicates single-binary distribution reasoning |

## Decision

**Apache-2.0** for all first-party code: engine, daemon, CLI, native host, browser extensions,
documentation, and the compatibility corpus.

Reasoning:

1. **The patent grant matters.** We implement HTTP/2, HTTP/3, and QUIC. An explicit patent
   grant is worth having and MIT does not provide one.
2. **The engine should be reusable.** A permissive licence means another project can build a
   different frontend, or embed the transfer engine, without a licence conversation. That is
   how the corpus and the engine end up benefiting more people than Downpour's own users.
3. **Distro and store friendliness.** Apache-2.0 is uncontroversial everywhere Downpour needs
   to be packaged.

### Consequence for dependencies

`cargo deny` enforces an Apache-2.0-compatible policy. GPL dependencies are excluded. This has
one live implication: **Slint's free open-source path is GPL-3.0** (see ADR-0002 and
`14-tech-radar-2026.md` §4). If Slint is chosen at Stage 10, `downpour-gui` becomes GPL-3.0
while everything else stays Apache-2.0 — which is legal and workable, but is a licence-story
cost that belongs in that decision.

### Consequence for study

aria2, gopeed, XDM, and Persepolis are GPL. We study their design; we do not copy code,
structure-by-structure translations, or test data from them into this repository.

## Consequences

**Easier:** anyone can use the engine; packaging is frictionless; contributions do not need a CLA.

**Harder:** a company could ship a proprietary product on our engine and contribute nothing.
That is accepted — the same permissiveness is what lets the engine and the corpus spread.

## Reversal trigger

None foreseen. Relicensing after contributions arrive requires every contributor's consent,
which is why this is settled in S0 rather than later.
