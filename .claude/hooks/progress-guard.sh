#!/usr/bin/env bash
# Stop hook — the RECORD-step enforcer from docs/agent/HARNESS.md.
#
# Blocks the turn from ending when substantive work happened but
# state/progress.json was not updated to reflect it.
#
# "Substantive" means a file under one of the watched paths is newer than
# state/progress.json. Prose edits under docs/ do not trigger it; ADRs do,
# because an ADR is a project-status change.
#
# Runs under both Claude Code and Codex — the Stop event and the
# {"decision":"block","reason":...} contract are the same on both.
#
# Any failure inside this script must NOT block: a broken guard that traps the
# agent is worse than no guard.

set -uo pipefail
# shellcheck source=_common.sh
. "$(dirname "${BASH_SOURCE[0]}")/_common.sh" 2>/dev/null || exit 0

PROGRESS="state/progress.json"
[ -f "$PROGRESS" ] || exit 0

WATCHED=(crates src extensions tests scripts docs/adr .claude/skills .claude/agents .claude/hooks plugins)

progress_mtime=$(stat -c %Y "$PROGRESS" 2>/dev/null || echo 0)
newer=""
for dir in "${WATCHED[@]}"; do
  [ -d "$dir" ] || continue
  while IFS= read -r f; do
    [ -n "$f" ] && newer+="  - ${f#./}"$'\n'
  done < <(find "$dir" -type f -newermt "@$progress_mtime" \
             ! -name '*.swp' ! -name '*~' ! -path '*/target/*' ! -path '*/node_modules/*' \
             2>/dev/null | head -12)
done

[ -z "$newer" ] && exit 0

json_escape() {
  if command -v jq >/dev/null 2>&1; then jq -Rs .
  else python3 -c 'import json,sys; print(json.dumps(sys.stdin.read()))'
  fi
}

reason=$(printf '%s' "These files changed but state/progress.json was not updated:

${newer}
HARNESS.md RECORD step: update state/progress.json in the same change.
Follow .claude/skills/progress-update/SKILL.md (readable as a plain procedure).
At minimum:
  - set the affected task statuses (a 'done' task needs a real 'proof')
  - append a session_log entry (at, agent: ${AGENT_ID}, stage, summary, files_touched)
  - set updated_at to now: $(date -u +%Y-%m-%dT%H:%M:%SZ)
Then run: node scripts/check-progress.mjs

If this change genuinely does not alter project status, say so in one line and
touch state/progress.json's updated_at to acknowledge it." | json_escape)

printf '{"decision":"block","reason":%s}\n' "$reason"
exit 0
