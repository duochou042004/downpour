# Downpour plugin marketplace

An in-repo Claude Code plugin marketplace: opt-in specialist toolkits for the stages that need
them.

## Why this exists separately from `.claude/skills/`

| | `.claude/skills/` | `plugins/` |
| --- | --- | --- |
| Loading | Automatic, always in context | Installed on demand |
| Contents | The project-critical loop: orient, prove, record, gate | Deep domain toolkits for one stage |
| Cost | Small, permanent | Zero until installed |

Everything an agent needs *every session* is in `.claude/`. Everything an agent needs *during
one stage* is here. Splitting them keeps the always-on context small, which matters more than
it sounds — a bloated permanent context is how agents end up skimming the rules that matter.

## Install

```bash
claude plugin marketplace add ./plugins
```

Then install what the current stage needs:

```bash
claude plugin install downpour-protocol@downpour-tools    # Stages 1, 5, 6
claude plugin install downpour-qa@downpour-tools          # Stages 2 onward — most of the project
claude plugin install downpour-release@downpour-tools     # Stage 10
```

`downpour-qa` is the one to install first and keep. From Stage 2 onward, most of the work is
correctness work.

## The plugins

### `downpour-protocol`

For HTTP, TLS, QUIC, and DNS work.

- `/probe-design` — design or review a capability probe against RFC 9110 range semantics
- `/protocol-decision` — decide the transfer strategy for an observed server behaviour
- `/rfc-check` — verify an assumption against the normative text before it becomes a bug
- Agent: `protocol-researcher`

### `downpour-qa`

For correctness. This is the plugin that carries ADR-0007.

- `/corpus-case` — write a compatibility corpus case from a described pathology
- `/sim-scenario` — write a deterministic simulation scenario with crash or timing adversity
- `/prop-test` — write a property test for a data structure's invariants
- `/bench-compare` — run and interpret the comparative benchmark against IDM, AB, aria2
- Agents: `corpus-author`, `test-adversary`

### `downpour-release`

For Stage 10. Ships **disabled by default** (`defaultEnabled: false`) — it is irrelevant for
nine stages and would only add noise.

- `/package-target` — build and verify a package for one target
- `/release-check` — walk the release checklist and report honestly what is not ready

## Codex and other agents

Codex does not support plugins or slash commands. Every skill here is written to be **readable
as a plain procedure** — open the `SKILL.md` and follow the steps. That is not a fallback, it
is how they are written: a skill that only makes sense as a command is a skill that has been
written badly.

## Adding a plugin

1. `plugins/<name>/.claude-plugin/plugin.json` with at least a `name`.
2. Skills in `plugins/<name>/skills/<skill-name>/SKILL.md`.
3. Agents in `plugins/<name>/agents/<agent>.md`.
4. Add an entry to `plugins/.claude-plugin/marketplace.json`.
5. Validate: `claude plugin validate ./plugins/<name> --strict`.

Keep them focused. A plugin that does four unrelated things is four plugins that have not been
separated yet.
