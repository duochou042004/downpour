# Contributing to Downpour

Thank you for considering it. This document is short; the depth is in
[`docs/`](docs/README.md).

## Before anything else

Downpour is a download **engine**. The part that can be silently wrong is much larger than the
part that can be visibly wrong. A change that makes a file 20% faster and corrupts it once in
ten thousand runs is a bad change, and the review process is built around that asymmetry.

Read [`docs/agent/HARNESS.md`](docs/agent/HARNESS.md) once. It is the operating manual for
humans and AI agents alike.

## Setup

```bash
git clone https://github.com/duochou042004/downpour.git
cd downpour
just setup      # installs the git hooks, then runs the environment check
just brief      # what stage we are in and what is open
```

`just setup` runs `git config core.hooksPath .githooks`. That is where this project enforces
its rules — see [Enforcement](#enforcement).

If `just doctor` reports missing tools, it prints the exact install commands. It never
installs anything itself.

## The workflow

```
develop ──┬── feature/short-description ──┐
          ├── fix/short-description ──────┼──▶ merge request ──▶ develop
          └── docs/short-description ─────┘

develop ──────────────────────────────────────▶ master   (releases only)
```

- **`master`** — release only. Nothing lands here except a tested release merge from `develop`.
- **`develop`** — integration. Everything merges here first and must be stable before it moves on.
- **`feature/*`, `fix/*`, `docs/*`, `chore/*`** — branch from `develop`, merge back via MR.

Never commit directly to `master`.

## Making a change

1. **Orient.** `just brief`. Confirm your change belongs to the current stage. If it belongs to
   a later stage, it does not get written yet — add it to `backlog` in
   [`state/progress.json`](state/progress.json) and say so.
2. **Write the failing test first.** Every behavioural claim needs a test that goes red when the
   behaviour is removed. Pick the layer from
   [`docs/09-testing-strategy.md`](docs/09-testing-strategy.md): property test for data
   structures, compatibility corpus case for server behaviour, deterministic simulation for
   timing and crash safety.
3. **Implement the smallest thing that passes.** Not the most general one.
4. **Run the gate.** `just gate`.
5. **Record it.** Update `state/progress.json` — task status, a real `proof`, a `session_log`
   entry. This is not bureaucracy; it is how the next contributor (or the next AI session)
   knows what is actually true.
6. **Open a merge request against `develop`.**

## Enforcement

`.githooks/pre-commit` blocks a commit that:

- stages a malformed `state/progress.json`,
- stages source, plugin, or ADR files **without** `state/progress.json`,
- stages agent assets that have drifted out of sync (`just sync` fixes it),
- fails `cargo fmt`, `cargo clippy -D warnings`, or `shellcheck`.

`--no-verify` exists. Using it routinely means the rules are wrong — say so in an issue rather
than working around them quietly.

## What gets a change rejected

- A behavioural claim with no test that would fail without it.
- A bug fix without a permanent regression case (see
  [ADR-0007](docs/adr/0007-corpus-as-primary-asset.md)).
- `unwrap()`, `expect()`, or `panic!` in engine or daemon code paths.
- Weakening an entry in [`docs/agent/INVARIANTS.md`](docs/agent/INVARIANTS.md) to make a test
  pass. If an invariant is genuinely wrong, that is an ADR, not an edit.
- An optimisation that reorders the durability sequence in
  [`docs/04-storage-and-recovery-spec.md`](docs/04-storage-and-recovery-spec.md) §2.3.
- A test that depends on a third-party server.
- Work that belongs to a later stage.

## Decisions

If you are choosing between two libraries, two data models, or two wire formats, write an ADR
in [`docs/adr/`](docs/adr/README.md) **before** the code. The template and rules are in
[`docs/adr/README.md`](docs/adr/README.md). An ADR without a "Reversal trigger" section is not
finished.

## Commits

Conventional Commits:

```
feat(engine): ETA-aware segment splitting
fix(storage): discard torn journal tail instead of erroring
test(corpus): add etag-changed-midway
docs(adr): 0010 choose interval map implementation
chore(ci): cache cargo registry
```

One logical change per commit. A commit touching the engine and the packaging scripts is two
commits.

## Merge request description

State: which stage, which exit criterion it advances, what test proves it, and what
`state/progress.json` diff accompanies it.

## Reporting bugs

Include the Downpour version, the OS, the URL pattern (redact tokens), and `dp explain <id>`
output if a download behaved oddly. If you can reproduce it, the most valuable thing you can
contribute is a corpus case — see
[`plugins/downpour-qa/skills/corpus-case/SKILL.md`](plugins/downpour-qa/skills/corpus-case/SKILL.md).

**Security issues do not go in the issue tracker.** See [`SECURITY.md`](SECURITY.md).

## Out of scope

Please read [`docs/00-vision-and-scorecard.md`](docs/00-vision-and-scorecard.md) §5 and
[ADR-0008](docs/adr/0008-no-drm-no-mitm.md) before proposing: DRM circumvention, TLS
interception, rate-limit evasion, torrents, or site-specific scrapers. These are settled
boundaries, not open questions.

## Licence

By contributing you agree your work is licensed under Apache-2.0. There is no CLA.
