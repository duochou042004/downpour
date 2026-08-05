#!/usr/bin/env bash
# Behavioural test for .claude/hooks/progress-guard.sh.
#
# The guard is the RECORD-step enforcer. It has two failure modes and they are not
# symmetric:
#
#   Too quiet — source changes without a progress record slip through. The project
#   memory drifts from reality and the next session builds on a foundation that is
#   not there.
#
#   Too loud  — it blocks a turn whose record WAS written. Every agent then learns
#   the warning is noise and dismisses it, which converts the guard into decoration
#   and reintroduces the first failure mode by another route.
#
# The second is what happened: `stat -c %Y` truncates to whole seconds, and
# `find -newermt "@<seconds>"` compares at nanosecond precision, so every file
# carrying a sub-second mtime — as `git checkout` and `git merge` produce — read as
# newer than state/progress.json, including state/progress.json itself.
#
# This test asserts both directions. It only ever changes mtimes, never content.

set -uo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)
cd "$ROOT" || exit 1

GUARD=".claude/hooks/progress-guard.sh"
PROGRESS="state/progress.json"
# Any watched file will do; this one is stable and cheap to touch.
WITNESS="docs/adr/README.md"

[ -f "$GUARD" ] || { echo "  progress-guard.sh not found, skipped"; exit 0; }

failures=0

# Restore both mtimes to now on the way out, whatever happened.
cleanup() { touch "$PROGRESS" "$WITNESS" 2>/dev/null || true; }
trap cleanup EXIT

blocks() {
  bash "$GUARD" 2>/dev/null | grep -q '"decision":"block"'
}

# ---- 1. A source file genuinely newer than the record must block.
touch "$PROGRESS"
sleep 0.01
touch "$WITNESS"
if blocks; then
  echo "  ok   blocks when a watched file is newer than the progress record"
else
  echo "  FAIL the guard did NOT block on an unrecorded source change."
  echo "       Source changes can now land with no progress record at all."
  failures=$((failures + 1))
fi

# ---- 2. Identical mtimes must NOT block.
#
# This is the regression. `git checkout` and `git merge --ff-only` stamp every file
# they write with one shared sub-second mtime, so a change whose record was written
# and committed alongside it lands here.
touch -r "$PROGRESS" "$WITNESS"
if blocks; then
  echo "  FAIL the guard blocked when the record has the SAME mtime as the source."
  echo "       This is the git-checkout case: it fires on a change that WAS recorded,"
  echo "       and a guard that cries wolf every session gets dismissed."
  failures=$((failures + 1))
else
  echo "  ok   stays quiet when the record shares the source's mtime"
fi

# ---- 3. A record newer than every source must not block.
sleep 0.01
touch "$PROGRESS"
if blocks; then
  echo "  FAIL the guard blocked with the progress record newer than every source."
  failures=$((failures + 1))
else
  echo "  ok   stays quiet when the record is newer than the source"
fi

if [ "$failures" -ne 0 ]; then
  echo "  progress-guard: $failures assertion(s) failed"
  exit 1
fi
echo "  progress-guard ok"
