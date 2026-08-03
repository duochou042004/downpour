#!/usr/bin/env bash
# Verifies that every relative Markdown link in the repository resolves to a real file.
#
# This matters more than it looks. The docs are the interface an AI agent uses to
# navigate the project: docs/agent/CONTEXT-MAP.md tells an agent which file to open
# for a task, and a broken pointer there sends it reading the wrong thing — or
# reading everything, which wastes the context it needs for the actual work.
#
# External (http/https) links are not checked: a network-dependent CI job that
# fails when someone else's site is down is a job that gets disabled.
#
# Usage: scripts/check-links.sh

set -uo pipefail
cd "$(dirname "$0")/.." || exit 1

broken=0
checked=0

while IFS= read -r file; do
  # Pull the target out of every ](...) that is not an external URL or a bare anchor.
  while IFS= read -r target; do
    [ -z "$target" ] && continue
    case "$target" in
      http://*|https://*|mailto:*|\#*) continue ;;
    esac
    # Strip any #fragment; we verify the file exists, not the anchor.
    path="${target%%#*}"
    [ -z "$path" ] && continue
    dir=$(dirname "$file")
    if [ ! -e "$dir/$path" ] && [ ! -e "$path" ]; then
      printf '  ✗ %s → %s\n' "$file" "$target"
      broken=$((broken + 1))
    fi
    checked=$((checked + 1))
  done < <(grep -oE '\]\([^)]+\)' "$file" 2>/dev/null | sed 's/^](//; s/)$//')
done < <(find . -name '*.md' \
           -not -path './node_modules/*' \
           -not -path './target/*' \
           -not -path './.git/*' \
           -not -path './plugins/downpour-harness/*' \
           2>/dev/null)

if [ "$broken" -gt 0 ]; then
  printf '\n✗ %d broken relative link(s) out of %d checked\n' "$broken" "$checked" >&2
  exit 1
fi

printf '✓ all %d relative markdown links resolve\n' "$checked"
exit 0
