#!/usr/bin/env bash
# Copyright 2026, The KyzoDB Authors. MPL-2.0.
#
# work-context.sh — generate .claude/focus-story.md from the KyzoDB Work board.
#
# The board is the only work state: Todo / In Progress / Done columns on
# GitHub project "KyzoDB Work" (#1). Epics are parent issues; a story is a
# sub-issue of its epic; sub-issue order is execution order. This script is
# read-only against GitHub and writes the one file the UserPromptSubmit hook
# injects: standing work-management rules, then three tiers bound to duties —
#
#   Todo column           name + description             (queued: clear runway)
#   In Progress column    full body + comments           (focus: execute)
#   focus epics' rest     name + description + condemned (upcoming: build
#                         in sub-issue order              toward; invest nothing
#                                                         in the condemned)
#
# TTL-throttled (no network per keystroke). Offline-safe: on any fetch failure
# keep the existing file, warn on stderr, exit 0 — a network blip must never
# wedge a prompt. Output is byte-stable for a given board state (no timestamps)
# so unchanged boards ride the prompt cache.
#
# Usage: scripts/work-context.sh [--force]

set -euo pipefail

ROOT="${CLAUDE_PROJECT_DIR:-$(git rev-parse --show-toplevel 2>/dev/null || echo .)}"
OWNER=kyzodb
REPO=kyzo
PROJECT=1
OUT="$ROOT/.claude/focus-story.md"
TTL=120

if [ "${1:-}" != "--force" ] && [ -s "$OUT" ]; then
  now=$(date +%s)
  mtime=$(stat -c %Y "$OUT" 2>/dev/null || echo 0)
  [ $((now - mtime)) -lt "$TTL" ] && exit 0
fi

fail() { echo "work-context: $1; keeping existing context" >&2; exit 0; }

items=$(gh project item-list "$PROJECT" --owner "$OWNER" --format json --limit 500 2>/dev/null) \
  || fail "board fetch failed"
open_issues=$(gh issue list --repo "$OWNER/$REPO" --state open --json number,title,body --limit 300 2>/dev/null) \
  || fail "issue fetch failed"

mapfile -t todo_nums < <(jq -r '.items[] | select(.status=="Todo") | .content.number // empty' <<<"$items")
mapfile -t focus_nums < <(jq -r '.items[] | select(.status=="In Progress") | .content.number // empty' <<<"$items")

# Print the named "## <Section>" of a story body, blank lines dropped.
section() {
  awk -v h="## $1" '$0 == h {f=1; next} /^## / {f=0} f && NF {print}'
}

# One GraphQL fetch per focus story: full body, comments, parent epic and the
# epic's sub-issues (number/title/state/body) in sub-issue order.
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
for n in "${focus_nums[@]:-}"; do
  [ -n "$n" ] || continue
  # shellcheck disable=SC2016  # $n is a GraphQL variable (bound by -F), not shell
  gh api graphql -F n="$n" -f query='
    query($n: Int!) {
      repository(owner: "kyzodb", name: "kyzo") {
        issue(number: $n) {
          number title body
          labels(first: 5) { nodes { name } }
          milestone { title }
          comments(last: 15) { nodes { author { login } createdAt body } }
          parent {
            number title
            subIssues(first: 100) { nodes { number title state body } }
          }
        }
      }
    }' > "$tmp/focus-$n.json" 2>/dev/null || fail "focus story #$n fetch failed"
done

{
  cat <<'RULES'
# KyzoDB Work Management

You track work on the board, and only there. Do not use your task manager;
tasks live in stories. Do not keep notes in your scratchpad; working notes are
tight, informative story comments. When a story's strategy evolves, rewrite
the body — do not append a contradicting comment trail.

The board must match reality every turn. A story you are working on is In
Progress; a completed story is moved to Done. Move the card the moment reality
changes — `scripts/move-story.sh <n> <todo|focus|done>` (done also closes the
issue). You do this to give yourself the safety operator oversight affords you.
RULES

  # ---- Queued -------------------------------------------------------------
  echo
  echo "## Queued on the board (Todo) — clear their runway"
  echo
  if [ "${#todo_nums[@]}" -eq 0 ]; then
    echo "(nothing queued)"
  else
    for n in "${todo_nums[@]}"; do
      title=$(jq -r --argjson n "$n" '.[] | select(.number==$n) | .title' <<<"$open_issues")
      [ -n "$title" ] || continue
      echo "### #$n — $title"
      jq -r --argjson n "$n" '.[] | select(.number==$n) | .body // ""' <<<"$open_issues" \
        | section "Description"
      echo
    done
  fi

  # ---- Focus ----------------------------------------------------------------
  echo "## Focus — execute this contract completely"
  echo
  if [ "${#focus_nums[@]}" -eq 0 ]; then
    echo "No story is in focus. Before any code work, confirm with the operator"
    echo "which story enters focus, then move it: scripts/move-story.sh <n> focus."
  else
    for n in "${focus_nums[@]}"; do
      f="$tmp/focus-$n.json"
      [ -s "$f" ] || continue
      jq -r '
        .data.repository.issue
        | "### #\(.number) — \(.title)",
          "Label: \([.labels.nodes[].name] | join(", ")) | Milestone: \(.milestone.title // "none") | Epic: \(if .parent then "#\(.parent.number) — \(.parent.title)" else "none" end)",
          "",
          (.body // ""),
          "",
          "#### Comments",
          (if (.comments.nodes | length) == 0 then "(none)" else
            (.comments.nodes[] | "**\(.author.login // "ghost") (\(.createdAt[:10])):**\n\(.body)\n") end)
      ' "$f"
      echo
    done
  fi

  # ---- Upcoming -------------------------------------------------------------
  echo "## Upcoming — the focus epics' remaining stories, in order. Build their"
  echo "## foundation now; invest nothing in what they condemn."
  echo
  if [ "${#focus_nums[@]}" -eq 0 ]; then
    echo "(no focus, so no upcoming)"
  else
    focus_set=$(printf '%s\n' "${focus_nums[@]}" | jq -R 'tonumber' | jq -s .)
    seen_epics=""
    for n in "${focus_nums[@]}"; do
      f="$tmp/focus-$n.json"
      [ -s "$f" ] || continue
      epic=$(jq -r '.data.repository.issue.parent.number // empty' "$f")
      [ -n "$epic" ] || continue
      case " $seen_epics " in *" $epic "*) continue ;; esac
      seen_epics="$seen_epics $epic"
      jq -r '.data.repository.issue.parent | "### Epic #\(.number) — \(.title)"' "$f"
      remaining=$(jq -r --argjson focus "$focus_set" '
        [.data.repository.issue.parent.subIssues.nodes[]
         | select(.state=="OPEN") | select(.number as $x | $focus | index($x) | not)]
        | length' "$f")
      if [ "$remaining" -eq 0 ]; then
        echo "(no remaining stories — this epic closes with the focus work)"
        echo
        continue
      fi
      while IFS= read -r sub_n; do
        jq -r --argjson s "$sub_n" '
          .data.repository.issue.parent.subIssues.nodes[]
          | select(.number==$s) | "#### #\(.number) — \(.title)"' "$f"
        body=$(jq -r --argjson s "$sub_n" '
          .data.repository.issue.parent.subIssues.nodes[]
          | select(.number==$s) | .body // ""' "$f")
        section "Description" <<<"$body"
        echo "Condemned:"
        section "Condemned" <<<"$body"
        echo
      done < <(jq -r --argjson focus "$focus_set" '
        .data.repository.issue.parent.subIssues.nodes[]
        | select(.state=="OPEN") | select(.number as $x | $focus | index($x) | not)
        | .number' "$f")
    done
    [ -n "$seen_epics" ] || echo "(the focus stories have no parent epic — attach them to their epics)"
  fi
} > "$OUT.tmp"
mv "$OUT.tmp" "$OUT"
