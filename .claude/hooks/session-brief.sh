#!/usr/bin/env bash
# SessionStart hook — orients a fresh agent session without it having to hunt.
#
# Emits the ORIENT step of docs/agent/HARNESS.md as additionalContext:
# current stage, open tasks, unmet criteria, and whether the environment is ready.
#
# Never blocks. Any failure exits silently — a broken brief must not break a session.

set -uo pipefail
# shellcheck source=_common.sh
. "$(dirname "${BASH_SOURCE[0]}")/_common.sh" 2>/dev/null || exit 0

PROGRESS="state/progress.json"
[ -f "$PROGRESS" ] || exit 0
command -v jq >/dev/null 2>&1 || exit 0

brief=$(jq -r '
  .current_stage as $cs
  | (.stages[] | select(.id == $cs)) as $s
  | [
      "DOWNPOUR — session brief (from state/progress.json)",
      "",
      "Stage \($cs): \($s.name)   [\($s.status)]",
      "Primary spec: \($s.spec // "docs/12-roadmap-stages.md")",
      "",
      "Open tasks:",
      ( [ $s.tasks[]? | select(.status == "in_progress" or .status == "todo" or .status == "blocked")
          | "  [\(.status)] \(.id)  \(.title)"
            + (if .notes then "\n         note: \(.notes)" else "" end)
            + (if .blocked_by then "\n         blocked by: \(.blocked_by)" else "" end) ]
        | if length == 0 then ["  (none — the stage may be ready for /stage-gate)"] else . end
        | join("\n") ),
      "",
      "Unmet exit criteria:",
      ( [ $s.exit_criteria[]? | select(.met | not) | "  \(.id)  \(.text)" ]
        | if length == 0 then ["  (all met — propose the gate with /stage-gate)"] else . end
        | join("\n") ),
      "",
      "Last session: \(.session_log[-1].summary // "none")",
      (if .session_log[-1].left_in_progress then "Left in progress: \(.session_log[-1].left_in_progress)" else empty end),
      "",
      "Reminders: read docs/agent/HARNESS.md before working. Stay inside \($cs).",
      "Write the failing test before the implementation. Update this file before you stop."
    ] | join("\n")
' "$PROGRESS" 2>/dev/null) || exit 0

[ -z "$brief" ] && exit 0

# Flag an unready environment once, at session start, rather than at first build failure.
if ! command -v cargo >/dev/null 2>&1; then
  brief+=$'\n\nNOTE: the Rust toolchain is not installed on this machine. Run `bash scripts/doctor.sh` for the exact commands. Do not attempt cargo builds until it is.'
fi

jq -nc --arg ctx "$brief" \
  '{hookSpecificOutput: {hookEventName: "SessionStart", additionalContext: $ctx}}'
exit 0
