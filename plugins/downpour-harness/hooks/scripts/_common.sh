#!/usr/bin/env bash
# Shared preamble for Downpour hook scripts.
#
# These hooks run under BOTH Claude Code (via .claude/settings.json, auto-loaded)
# and Codex (via the downpour-harness plugin). The two set different environment
# variables and invoke from different working directories, so every hook sources
# this file first and then relies only on what it establishes.
#
# Establishes: REPO_ROOT (and cd's there), PATH including ~/.cargo/bin, AGENT_ID.
# Never exits non-zero — a hook that breaks the session is worse than no hook.

# Resolve the repository root, in decreasing order of reliability.
_resolve_repo_root() {
  local d
  for d in "${CLAUDE_PROJECT_DIR:-}" "${CODEX_PROJECT_DIR:-}" "${PROJECT_DIR:-}"; do
    if [ -n "$d" ] && [ -f "$d/state/progress.json" ]; then printf '%s' "$d"; return 0; fi
  done
  if d=$(git rev-parse --show-toplevel 2>/dev/null) && [ -f "$d/state/progress.json" ]; then
    printf '%s' "$d"; return 0
  fi
  # Walk up from the script's own location: works when a plugin invokes us by
  # absolute path from an unrelated cwd.
  d=$(cd "$(dirname "${BASH_SOURCE[0]}")" 2>/dev/null && pwd -P) || return 1
  while [ "$d" != "/" ]; do
    if [ -f "$d/state/progress.json" ]; then printf '%s' "$d"; return 0; fi
    d=$(dirname "$d")
  done
  [ -f "./state/progress.json" ] && { printf '%s' "$PWD"; return 0; }
  return 1
}

REPO_ROOT=$(_resolve_repo_root) || return 0 2>/dev/null || exit 0
cd "$REPO_ROOT" 2>/dev/null || return 0 2>/dev/null || exit 0

# Non-interactive shells never source the rustup profile line.
[ -d "$HOME/.cargo/bin" ] && case ":$PATH:" in
  *":$HOME/.cargo/bin:"*) ;;
  *) PATH="$HOME/.cargo/bin:$PATH"; export PATH ;;
esac

# Best-effort identification, used for the session_log 'agent' field.
if [ -n "${CLAUDE_PROJECT_DIR:-}" ] || [ -n "${CLAUDE_SESSION_ID:-}" ]; then
  AGENT_ID="claude-code"
elif [ -n "${CODEX_HOME:-}" ] || [ -n "${CODEX_PROJECT_DIR:-}" ]; then
  AGENT_ID="codex"
else
  AGENT_ID="other-agent"
fi
export REPO_ROOT AGENT_ID
