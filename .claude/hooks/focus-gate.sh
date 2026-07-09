#!/usr/bin/env bash
# PreToolUse(Edit|Write) hook: no focus story, no engine edits.
# Denies edits under any kyzo-* directory unless at least one open issue
# carries the "focus" label (move-issue --column focus maintains the
# invariant that focus-labeled stories are In Progress). The check is
# cached briefly so it does not tax every edit. Fails OPEN on gh failure:
# a broken network must not brick the session; the board injection still
# surfaces the missing focus story.
set -uo pipefail

ROOT="${CLAUDE_PROJECT_DIR:-$(git rev-parse --show-toplevel 2>/dev/null || echo .)}"
CACHE="$ROOT/.claude/.focus-gate-cache"
TTL=60

file=$(jq -r '.tool_input.file_path // ""')
case "$file" in
  *kyzo-core/*|*kyzo-bin/*|*kyzo-crashfs/*|*kyzo-lsp/*|*kyzo-arrow-interop/*|*kyzo-model/*|*kyzo-oracle/*|*kyzo-trials/*|*kyzo-wasm/*) ;;
  *) exit 0 ;;
esac

fresh=0
if [ -f "$CACHE" ]; then
  age=$(($(date +%s) - $(stat -c %Y "$CACHE" 2>/dev/null || echo 0)))
  [ "$age" -lt "$TTL" ] && fresh=1
fi
if [ "$fresh" -eq 0 ]; then
  count=$(gh issue list --repo kyzodb/kyzo --state open --label focus --json number --jq length 2>/dev/null) \
    || exit 0 # fail open: gh unavailable
  printf '%s' "$count" >"$CACHE"
fi

count=$(cat "$CACHE" 2>/dev/null || echo 0)
if [ "${count:-0}" -eq 0 ]; then
  jq -cn '{hookSpecificOutput:{hookEventName:"PreToolUse",permissionDecision:"deny",permissionDecisionReason:"No story is in focus (In Progress + the focus label) — engine code changes require one. Run: .claude/skills/manage-board/manage-board.py move-issue <n> --column focus, or get the operator to pick the story."}}'
fi
exit 0
