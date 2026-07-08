#!/usr/bin/env bash
# UserPromptSubmit hook: give the agent its bearings each turn (#140).
# Refreshes the generated board context (scripts/board-context — read-only,
# TTL-throttled, offline-safe) and injects the three files: active story,
# next work, board signal. Context and nudges only; this hook never blocks
# anything and the tooling never mutates board state.
set -euo pipefail

root="${CLAUDE_PROJECT_DIR:-$(git rev-parse --show-toplevel 2>/dev/null || echo .)}"

# Consume stdin (the hook JSON) so the pipe never blocks; we don't need it.
cat >/dev/null 2>&1 || true

if [ -x "$root/scripts/board-context" ]; then
  "$root/scripts/board-context" >/dev/null 2>&1 || true
fi

story="$root/.claude/active-story.md"
[ -s "$story" ] || exit 0

msg="ACTIVE STORY (from .claude/active-story.md — obey .claude/rules/ as law):

$(cat "$story")"

for f in next-work board-signal; do
  p="$root/.claude/$f.md"
  [ -s "$p" ] || continue
  msg="$msg

--- $(tr 'a-z-' 'A-Z ' <<<"$f") (from .claude/$f.md) ---
$(cat "$p")"
done

jq -cn --arg m "$msg" \
  '{hookSpecificOutput:{hookEventName:"UserPromptSubmit",additionalContext:$m}}'
