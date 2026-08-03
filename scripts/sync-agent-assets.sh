#!/usr/bin/env bash
# Keeps the Codex plugin's copy of the skills and hook scripts identical to the
# canonical ones in .claude/.
#
# WHY THIS EXISTS
#   .claude/skills and .claude/hooks are the single source of truth: Claude Code
#   loads them from the project directory in place, with no install step.
#   Codex loads skills from an *installed plugin*, and `codex plugin add` copies
#   the plugin into a cache — a copy that does NOT follow symlinks. So the plugin
#   needs real files.
#
#   Rather than maintain two hand-edited copies (which silently drift, and then
#   the two agents are working from different rules), the plugin copy is
#   generated from the canonical one and CI fails if they diverge.
#
# Usage:
#   scripts/sync-agent-assets.sh            # regenerate the plugin copy
#   scripts/sync-agent-assets.sh --check    # exit 1 if out of sync (used in CI)

set -euo pipefail
cd "$(dirname "$0")/.." || exit 1

SRC_SKILLS=".claude/skills"
SRC_HOOKS=".claude/hooks"
DST_SKILLS="plugins/downpour-harness/skills"
DST_HOOKS="plugins/downpour-harness/hooks/scripts"

CHECK=0
[ "${1:-}" = "--check" ] && CHECK=1

sync_dir() {
  local src="$1" dst="$2"
  if [ "$CHECK" -eq 1 ]; then
    if [ ! -d "$dst" ]; then
      echo "✗ $dst is missing — run scripts/sync-agent-assets.sh" >&2
      return 1
    fi
    # README.md in the destination is the generated do-not-edit marker; it has no
    # counterpart in the source and must not count as drift.
    if ! diff -r -q -x README.md "$src" "$dst" >/dev/null 2>&1; then
      echo "✗ $dst has drifted from $src:" >&2
      diff -r -q -x README.md "$src" "$dst" >&2 || true
      echo "  Run: scripts/sync-agent-assets.sh" >&2
      return 1
    fi
  else
    rm -rf "$dst"
    mkdir -p "$(dirname "$dst")"
    cp -R "$src" "$dst"
  fi
}

rc=0
sync_dir "$SRC_SKILLS" "$DST_SKILLS" || rc=1
sync_dir "$SRC_HOOKS"  "$DST_HOOKS"  || rc=1

if [ "$CHECK" -eq 1 ]; then
  [ "$rc" -eq 0 ] && echo "✓ agent assets are in sync"
  exit "$rc"
fi

# Mark the generated tree so nobody edits it by hand.
cat > "$DST_SKILLS/README.md" <<'INNER'
# Generated — do not edit

These files are copied from `.claude/skills/` by `scripts/sync-agent-assets.sh`.

`.claude/skills/` is the canonical source. Claude Code reads it in place; Codex needs
real files inside an installed plugin, and `codex plugin add` does not follow symlinks.

Edit `.claude/skills/`, then run `scripts/sync-agent-assets.sh`. CI runs the `--check`
mode and fails if these drift apart.
INNER
chmod +x "$DST_HOOKS"/*.sh 2>/dev/null || true
echo "✓ synced $SRC_SKILLS → $DST_SKILLS"
echo "✓ synced $SRC_HOOKS  → $DST_HOOKS"
