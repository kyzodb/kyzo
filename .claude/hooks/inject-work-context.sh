#!/usr/bin/env bash
# Copyright 2026, The KyzoDB Authors. MPL-2.0.
#
# inject-work-context.sh — UserPromptSubmit hook; the whole work-context
# generator in one script.
#
#   focus set   = open issues In Progress on the board AND carrying the
#                 "focus" label (the label is the one focus authority)
#   IN PROGRESS = "In Progress" column minus the focus set — other sessions'
#                 live work, injected so it is never clobbered: name + Description
#   FOCUS       = each focus story: full body + comments
#   UPCOMING    = the focus stories' parent epics -> their remaining OPEN
#                 sub-issues, in sub-issue order, minus the focus set:
#                 name + Description + Condemned. Nothing else may enter.
#
# Output = work-context-template.sh with IN_PROGRESS_STORIES,
# FOCUS_STORIES, UPCOMING_STORIES expanded. Cached 120s at
# .claude/.work-context-cache.md. On any fetch failure emit the stale cache —
# stale beats invented — and always exit 0: never block a prompt.

set -uo pipefail # no -e: failures degrade to the cache, never abort the hook

ROOT="${CLAUDE_PROJECT_DIR:-$(git rev-parse --show-toplevel 2>/dev/null || echo .)}"
OWNER=kyzodb
REPO=kyzo
PROJECT=1
TEMPLATE="$ROOT/.claude/hooks/work-context-template.sh"
CACHE="$ROOT/.claude/.work-context-cache.md"
TTL=120

cat >/dev/null 2>&1 || true # consume the hook's stdin JSON

emit_cache_and_exit() {
  [ -s "$CACHE" ] && cat "$CACHE"
  exit 0
}

if [ -s "$CACHE" ]; then
  age=$(($(date +%s) - $(stat -c %Y "$CACHE" 2>/dev/null || echo 0)))
  [ "$age" -lt "$TTL" ] && emit_cache_and_exit
fi

[ -f "$TEMPLATE" ] || emit_cache_and_exit

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

items=$(gh project item-list "$PROJECT" --owner "$OWNER" --format json --limit 500 2>/dev/null) \
  || emit_cache_and_exit
open_issues=$(gh issue list --repo "$OWNER/$REPO" --state open --json number,title,body --limit 300 2>/dev/null) \
  || emit_cache_and_exit
labeled=$(gh issue list --repo "$OWNER/$REPO" --state open --label focus --json number --limit 100 2>/dev/null) \
  || emit_cache_and_exit

# Focus set: In Progress on the board AND carrying the focus label.
mapfile -t FOCUS < <(jq -r --argjson lab "$labeled" '.items[]
  | select(.status=="In Progress")
  | .content.number // empty
  | select(. as $x | $lab | map(.number) | index($x) != null)' <<<"$items" | awk '!seen[$0]++')

# Print the body's "## <name>" section, blank lines dropped.
section() { awk -v h="## $1" '$0==h{f=1;next} /^## /{f=0} f&&NF'; }

# ---- IN_PROGRESS_STORIES: In Progress column minus focus, name + Description
focus_filter=$(printf '%s\n' "${FOCUS[@]:-}" | jq -R 'select(length>0) | tonumber' | jq -s .)
: >"$tmp/inprogress.md"
while IFS= read -r n; do
  [ -n "$n" ] || continue
  title=$(jq -r --argjson n "$n" '.[] | select(.number==$n) | .title // empty' <<<"$open_issues")
  [ -n "$title" ] || continue
  {
    echo "#$n $title"
    jq -r --argjson n "$n" '.[] | select(.number==$n) | .body // ""' <<<"$open_issues" | section "Description"
    echo
  } >>"$tmp/inprogress.md"
done < <(jq -r --argjson fs "$focus_filter" '.items[]
  | select(.status=="In Progress")
  | .content.number // empty
  | select(. as $x | $fs | index($x) | not)' <<<"$items")
[ -s "$tmp/inprogress.md" ] || echo "(no other work is in progress)" >"$tmp/inprogress.md"

# ---- FOCUS_STORIES + UPCOMING_STORIES --------------------------------------
: >"$tmp/focus.md"
: >"$tmp/upcoming.md"

if [ "${#FOCUS[@]}" -eq 0 ]; then
  cat >"$tmp/focus.md" <<'EOF'
No story is in focus right now (In Progress + the "focus" label). That is a
valid state — but you frequently fail to mark the story you are actually
working on: if this conversation is working on a story, call the
move_to_in_progress MCP tool (manage-board skill) on it before continuing,
so the board reflects the current state of work.
EOF
  echo "(upcoming derives from the focus stories' epics — none in focus)" >"$tmp/upcoming.md"
else
  focus_json=$(printf '%s\n' "${FOCUS[@]}" | jq -s 'map(tonumber)')
  seen_epics=" "
  for n in "${FOCUS[@]}"; do
    # shellcheck disable=SC2016  # $n is a GraphQL variable (bound by -F), not shell
    gh api graphql -F n="$n" -f query='
      query($n: Int!) {
        repository(owner: "kyzodb", name: "kyzo") {
          issue(number: $n) {
            number title body
            comments(last: 15) { nodes { author { login } createdAt body } }
            parent {
              number title
              subIssues(first: 100) { nodes { number title state body } }
            }
          }
        }
      }' >"$tmp/f$n.json" 2>/dev/null \
      || {
        echo "#$n (could not fetch this focus story)" >>"$tmp/focus.md"
        echo >>"$tmp/focus.md"
        continue
      }

    jq -r '.data.repository.issue
      | "#\(.number) \(.title)",
        "",
        (.body // ""),
        "",
        "Comments:",
        (if (.comments.nodes | length) == 0 then "(none)"
         else (.comments.nodes[] | "— \(.author.login // "ghost") (\(.createdAt[:10])): \(.body)") end),
        ""' "$tmp/f$n.json" >>"$tmp/focus.md"

    epic=$(jq -r '.data.repository.issue.parent.number // empty' "$tmp/f$n.json")
    [ -n "$epic" ] || continue
    case "$seen_epics" in *" $epic "*) continue ;; esac
    seen_epics="$seen_epics$epic "

    while IFS= read -r sub; do
      [ -n "$sub" ] || continue
      body=$(jq -r --argjson s "$sub" '.data.repository.issue.parent.subIssues.nodes[]
        | select(.number==$s) | .body // ""' "$tmp/f$n.json")
      {
        jq -r --argjson s "$sub" '.data.repository.issue.parent.subIssues.nodes[]
          | select(.number==$s) | "#\(.number) \(.title)"' "$tmp/f$n.json"
        section "Description" <<<"$body"
        section "Condemned" <<<"$body"
        echo
      } >>"$tmp/upcoming.md"
    done < <(jq -r --argjson fs "$focus_json" '.data.repository.issue.parent.subIssues.nodes[]
      | select(.state=="OPEN")
      | select(.number as $x | $fs | index($x) | not)
      | .number' "$tmp/f$n.json")
  done
  [ -s "$tmp/upcoming.md" ] || echo "(the focus stories' epics have no remaining stories)" >"$tmp/upcoming.md"
fi

# ---- render the template (bash heredoc; the three variables expand) --------
IN_PROGRESS_STORIES="$(cat "$tmp/inprogress.md")" \
  FOCUS_STORIES="$(cat "$tmp/focus.md")" \
  UPCOMING_STORIES="$(cat "$tmp/upcoming.md")" \
  bash "$TEMPLATE" >"$CACHE.tmp" && mv "$CACHE.tmp" "$CACHE"

cat "$CACHE"
exit 0
