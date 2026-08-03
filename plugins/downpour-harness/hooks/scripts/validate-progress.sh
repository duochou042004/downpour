#!/usr/bin/env bash
# PostToolUse hook — validates state/progress.json immediately after it changes.
#
# Catching a malformed progress file at the moment of the edit is far cheaper
# than catching it in CI, and far cheaper than a later session reading it.
#
# Agent-agnostic: Claude Code reports the edited path in .tool_input.file_path,
# Codex uses apply_patch and shell calls whose shape differs. Rather than parse
# every variant, this triggers when the payload mentions progress.json OR when
# the file changed in the last 15 seconds. Validation is cheap; a missed
# validation is not.

set -uo pipefail
# shellcheck source=_common.sh
. "$(dirname "${BASH_SOURCE[0]}")/_common.sh" 2>/dev/null || exit 0

PROGRESS="state/progress.json"
[ -f "$PROGRESS" ] || exit 0
command -v node >/dev/null 2>&1 || exit 0
[ -f scripts/check-progress.mjs ] || exit 0

payload=$(cat 2>/dev/null || true)

mentions=0
case "$payload" in *progress.json*) mentions=1 ;; esac

recently_changed=0
mtime=$(stat -c %Y "$PROGRESS" 2>/dev/null || echo 0)
now=$(date +%s)
[ $((now - mtime)) -le 15 ] && recently_changed=1

[ "$mentions" -eq 0 ] && [ "$recently_changed" -eq 0 ] && exit 0

if out=$(node scripts/check-progress.mjs 2>&1); then
  exit 0
fi

json_escape() {
  if command -v jq >/dev/null 2>&1; then jq -Rs .
  else python3 -c 'import json,sys; print(json.dumps(sys.stdin.read()))'
  fi
}

reason=$(printf '%s' "state/progress.json failed validation:

$out

Fix these before continuing. The schema is state/progress.schema.json and the
project rules are documented at the top of scripts/check-progress.mjs." | json_escape)

printf '{"decision":"block","reason":%s}\n' "$reason"
exit 0
