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

# 3. Clearing requires BOTH a fresh local gate report AND GitHub-visible
#    completion evidence (#140): an evidence comment on the active story issue.
#    The board is the work authority — a green local report is not completion if
#    the board shows nothing. The GitHub check is best-effort: if we cannot reach
#    GitHub, we do not wedge (treat as satisfied), we only ENFORCE when we can
#    positively see the board lacks evidence.
report="$root/.claude/current-gate-report.md"
gate_ok=0
if [ -f "$report" ] && [ "$report" -nt "$story" ]; then
  gate_ok=1
fi

evidence_ok="unknown"
active=$(grep -m1 '^active_story:' "$story" 2>/dev/null | sed -E 's/^active_story:[[:space:]]*#?//' | tr -d '[:space:]')
case "$active" in
  ''|none|ambiguous) : ;;
  *)
    if bodies=$(gh api "repos/{owner}/{repo}/issues/$active/comments" --paginate --jq '.[].body' 2>/dev/null); then
      if printf '%s' "$bodies" | grep -q 'board-story-evidence'; then
        evidence_ok="yes"
      else
        evidence_ok="no"
      fi
    fi
    ;;
esac

# Clear when the local gate is fresh AND the board evidence is present (or the
# board was unreachable — never wedge offline).
if [ "$gate_ok" = 1 ] && { [ "$evidence_ok" = "yes" ] || [ "$evidence_ok" = "unknown" ]; }; then
  exit 0
fi

reason="You are stopping mid-story with uncommitted work and incomplete completion evidence. Before reporting completion, (a) produce a FACTS-ONLY gate ledger (00-story-gates.md / 02-final-report.md): commit range, exact commands, test counts (pass/fail), ignored count, clippy/fmt (own vs vendored), both feature configs, benchmark result if perf touched, compile-fail result if authority touched, remaining-red ledger (01-no-deferral.md) — write it to .claude/current-gate-report.md; AND (b) post GitHub-visible evidence to the active story with scripts/board-story-evidence (the board is the work authority, #140). Or set the story status to paused/done in .claude/active-story.md. No success language until gates pass."
if [ "$gate_ok" = 1 ] && [ "$evidence_ok" = "no" ]; then
  reason="Local gate report is fresh, but the board shows NO completion evidence on active story #$active (#140). Completion requires GitHub-visible evidence: run scripts/board-story-evidence to post the branch/commits/gate/test ledger to the issue, then stop. The board is the work authority — a green local report is not completion."
fi

jq -cn --arg r "$reason" '{decision:"block",reason:$r}'
