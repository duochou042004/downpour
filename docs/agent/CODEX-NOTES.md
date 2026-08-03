# Codex Notes

Everything in [`AGENTS.md`](../../AGENTS.md) applies. This file covers the Codex-specific
mechanics only.

Codex is a first-class agent on this project, not a fallback. The two-agent setup is
deliberate: Claude Code and Codex have different failure modes, and having both read the same
`AGENTS.md`, the same skills, and the same `state/progress.json` means a mistake by one is
often caught by the other.

> **Verified against Codex CLI 0.146.0 on 2026-08-03.** Every claim below was tested on this
> machine, not inferred from documentation. Re-verify after a Codex upgrade — the plugin and
> hook surfaces have been moving.

---

## 1. What Codex gets, and how

| Capability | Status | Mechanism |
| ---------- | ------ | --------- |
| Project instructions | **Works** | `AGENTS.md` at the repo root, loaded automatically |
| All 9 project skills | **Works** | The `downpour-tools` marketplace at [`.agents/plugins/marketplace.json`](../../.agents/plugins/marketplace.json) |
| Per-project config | **Works** | [`.codex/config.toml`](../../.codex/config.toml) |
| Progress gate enforcement | **Works** | `.githooks/pre-commit` — not a Codex feature, see §4 |
| Plugin-provided hooks | **Removed in 0.146** | `codex features list` → `plugin_hooks … removed`. Do not declare `hooks` in a Codex plugin manifest; it is silently ignored. |
| Session-start briefing | **Not available** | Claude Code injects it via a `SessionStart` hook. Codex sessions must run `just brief` manually — see §3. |

Verification, if you want to confirm it yourself:

```bash
codex debug prompt-input | grep -oE '(progress-update|context-brief|stage-gate|invariant-check|adr-new)'
```

That prints the skill names actually loaded into the model's prompt. If it prints nothing, the
marketplace is not registered — go to §2.

## 2. One-time setup

```bash
just init-agents
```

That registers the repo as a plugin marketplace for both agents. Or, for Codex alone:

```bash
codex plugin marketplace add . && codex plugin add downpour-harness@downpour-tools
```

Install `downpour-qa@downpour-tools` too — from Stage 2 onward, most of the work is
correctness work. `downpour-protocol` for Stages 1, 5 and 6; `downpour-release` for Stage 10.

Check what is installed:

```bash
codex plugin list
```

### Where the skills physically live

`.claude/skills/` is the **canonical source**. Claude Code reads it from the project directory
in place, with no install step.

Codex loads skills from an *installed plugin*, and `codex plugin add` copies the plugin into
`~/.codex/plugins/cache/` — a copy that **does not follow symlinks**. So the plugin needs real
files, and `plugins/downpour-harness/skills/` is a generated copy.

Never edit the generated copy. Edit `.claude/skills/`, then:

```bash
just sync
```

`just sync-check` verifies they match, and the pre-commit hook and CI both run it. This is why
you cannot silently end up with two agents following two different rulebooks.

## 3. The one thing that is genuinely weaker under Codex

Claude Code gets a `SessionStart` hook that injects the current stage, open tasks, unmet exit
criteria, and the last session's handoff note. **Codex gets nothing automatically.**

So start every Codex session with:

```bash
just brief
```

It prints exactly what the Claude Code hook injects. Skipping it is how a Codex session ends up
working on the wrong stage.

## 4. Enforcement is at the commit, not the turn

Claude Code blocks a turn from ending when source changed without a `state/progress.json`
update. Codex has no equivalent, and `plugin_hooks` is removed, so there is no way to reproduce
it inside the agent.

The project therefore enforces its rules at the **commit boundary**, where every agent and every
human has to pass through:

```bash
just init-hooks     # git config core.hooksPath .githooks
```

`.githooks/pre-commit` blocks a commit that:

1. stages a malformed `state/progress.json`,
2. stages source, plugin, or ADR files **without** `state/progress.json`,
3. stages agent assets that have drifted out of sync,
4. fails `cargo fmt`, `cargo clippy -D warnings`, or `shellcheck`.

This is a better design than an agent-specific hook anyway — it is one rule, enforced in one
place, for everyone. Run `just init-hooks` after cloning; it is not automatic.

## 5. Session checklist

Start:

```bash
just brief
```

Before you finish:

```bash
just gate
```

`just gate` runs the progress validator, the asset sync check, shellcheck, and — once the
workspace exists — `fmt`, `clippy -D warnings`, `nextest`, and doctests.

Then state explicitly in your final message:

- which task ids changed status,
- what test proves the change,
- that `session_log` has a new entry,
- anything left `in_progress` and exactly where you stopped.

A Codex session that ends without a `session_log` entry leaves the next session — of either
agent — flying blind. That is the single easiest thing to forget here.

## 6. Sandbox and approvals

[`.codex/config.toml`](../../.codex/config.toml) sets `approval_policy = "on-request"` and
`sandbox_mode = "workspace-write"` with network access enabled. Network is for fetching crates
and running the local corpus server; Downpour's tests never reach a third-party server
(`docs/09-testing-strategy.md` §7).

The corpus server and the simulation suite are entirely local and work in a cloud or remote
Codex environment. Anything needing a real browser or a display server — Stage 7 end-to-end
tests, Stage 10 GUI — belongs to a local session.

## 7. Nested AGENTS.md

Codex merges `AGENTS.md` from the repo root down to the file being edited, closest file
winning, with explicit user instructions overriding all of them.

Downpour keeps a single root `AGENTS.md` on purpose. If a stage adds a scoped one — say
`crates/downpour-engine/AGENTS.md` — it must **narrow**, never contradict, the root file. The
boundaries in `AGENTS.md` §"Security considerations" cannot be relaxed by a nested file. If you
find yourself writing an exception to a boundary in a nested `AGENTS.md`, something has gone
wrong upstream.

## 8. MCP

Codex supports MCP over stdio and streamable HTTP, configured in `config.toml`. This project
requires no MCP server. If a stage adds one — for example a local server that queries the
compatibility corpus — declare it in `.codex/config.toml` and write an ADR, because it becomes
part of how the project is built.

## 9. Re-verification

The Codex plugin and hook surfaces changed between 0.145 and 0.146 (`plugin_hooks` was
removed). After any Codex upgrade, re-run:

```bash
codex features list | grep -E 'hooks|plugins|skill'
```

If `plugin_hooks` returns to `stable`, we can move the guardrails into the plugin and drop the
reliance on git hooks for Codex. Until then, §4 is the mechanism. Record the finding in this
file and in `state/progress.json`.
