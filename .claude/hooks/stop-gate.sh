#!/usr/bin/env bash
# Stop hook: refuse to stop with completion language while a story is actively
# in flight and work is uncommitted. Deliberately narrow so it NEVER wedges
# normal conversation:
#
#   - honors stop_hook_active (no infinite loop)
#   - silent unless .claude/active-story.md has `status: in-progress`
#   - silent when the git tree is clean (work is landed/committed)
#   - clearable by committing the work, producing .claude/current-gate-report.md,
#     or flipping the story status.
set -euo pipefail

root="${CLAUDE_PROJECT_DIR:-$(git rev-parse --show-toplevel 2>/dev/null || echo .)}"
input=$(cat 2>/dev/null || echo '{}')

# 1. Never loop.
if [ "$(printf '%s' "$input" | jq -r '.stop_hook_active // false')" = "true" ]; then
  exit 0
fi

story="$root/.claude/active-story.md"
[ -f "$story" ] || exit 0
grep -Eqi '^[[:space:]]*status:[[:space:]]*in-progress' "$story" || exit 0

# 2. Only bite when there is uncommitted work in flight.
cd "$root" 2>/dev/null || exit 0
dirty=$(git status --porcelain 2>/dev/null || true)
[ -n "$dirty" ] || exit 0

# 3. A fresh gate report clears the bite.
report="$root/.claude/current-gate-report.md"
if [ -f "$report" ] && [ "$report" -nt "$story" ]; then
  exit 0
fi

jq -cn '{decision:"block",reason:"You are stopping mid-story with uncommitted work and no fresh gate report. Before reporting completion, produce a FACTS-ONLY gate ledger (00-story-gates.md / 02-final-report.md): commit range, exact commands, test counts (pass/fail), ignored count, clippy/fmt (own vs vendored), both feature configs, benchmark result if perf touched, compile-fail result if authority touched, and the remaining-red ledger (01-no-deferral.md). Either commit the work and write .claude/current-gate-report.md, or set the story status to paused/done in .claude/active-story.md. No success language until gates pass."}'
