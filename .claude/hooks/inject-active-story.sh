#!/usr/bin/env bash
# UserPromptSubmit hook: inject the active story's constraints as context each
# turn. Reads a SHORT file (.claude/active-story.md) — not the whole
# architecture. Silent (exit 0, no output) when there is no active story.
#
# UserPromptSubmit can only ADD context; it cannot replace the prompt.
set -euo pipefail

root="${CLAUDE_PROJECT_DIR:-$(git rev-parse --show-toplevel 2>/dev/null || echo .)}"
story="$root/.claude/active-story.md"

# Consume stdin (the hook JSON) so the pipe never blocks; we don't need it.
cat >/dev/null 2>&1 || true

[ -f "$story" ] || exit 0

body=$(cat "$story")
[ -n "$body" ] || exit 0

msg="ACTIVE STORY CONSTRAINTS (from .claude/active-story.md — obey .claude/rules/ as law):

$body"

jq -cn --arg m "$msg" \
  '{hookSpecificOutput:{hookEventName:"UserPromptSubmit",additionalContext:$m}}'
