#!/usr/bin/env bash
# UserPromptSubmit hook: inject the generated board context each turn (#140 v2).
# The active story is defined on the BOARD: the one open issue carrying the
# `active-story` label. This hook first refreshes the local caches from GitHub
# (active-story-sync, TTL-throttled and offline-safe — it also regenerates
# outcome-context and version-horizon), then injects the generated files in
# context-policy order: active story, outcome arc, version horizon. Silent
# (exit 0, no output) when there is no generated context.
#
# UserPromptSubmit can only ADD context; it cannot replace the prompt.
set -euo pipefail

root="${CLAUDE_PROJECT_DIR:-$(git rev-parse --show-toplevel 2>/dev/null || echo .)}"
story="$root/.claude/active-story.md"
outcome="$root/.claude/outcome-context.md"
horizon="$root/.claude/version-horizon.md"

# Consume stdin (the hook JSON) so the pipe never blocks; we don't need it.
cat >/dev/null 2>&1 || true

# Refresh the generated caches from GitHub (the board is authoritative). This
# is TTL-throttled and keeps the existing caches on any gh/network failure, so
# it never wedges a prompt. Best-effort: a sync failure must not fail the hook.
if [ -x "$root/scripts/active-story-sync" ]; then
  "$root/scripts/active-story-sync" >/dev/null 2>&1 || true
fi

[ -f "$story" ] || exit 0

body=$(cat "$story")
[ -n "$body" ] || exit 0

msg="ACTIVE STORY CONSTRAINTS (from .claude/active-story.md — obey .claude/rules/ as law):

$body"

if [ -f "$outcome" ]; then
  msg="$msg

--- OUTCOME ARC (from .claude/outcome-context.md) ---
$(cat "$outcome")"
fi

if [ -f "$horizon" ]; then
  msg="$msg

--- VERSION HORIZON (from .claude/version-horizon.md) ---
$(cat "$horizon")"
fi

jq -cn --arg m "$msg" \
  '{hookSpecificOutput:{hookEventName:"UserPromptSubmit",additionalContext:$m}}'
