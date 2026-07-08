#!/usr/bin/env bash
# Copyright 2026, The KyzoDB Authors. MPL-2.0.
#
# move-story.sh — the one board verb.
#
#   scripts/move-story.sh <issue-number> <todo|focus|done>
#
# Keeps the two representations true in one motion:
#   focus-story.md  (the focus authority: integer lines) — focus adds the
#                   number; todo/done remove it
#   the board       (the operator's picture) — todo -> Todo,
#                   focus -> In Progress, done -> Done (+ close the issue)
# Adds the card if the issue is not on the board. Invalidates the injected
# work-context cache so the next prompt reflects the move.

set -euo pipefail

OWNER=kyzodb
REPO=kyzo
PROJECT=1
ROOT="${CLAUDE_PROJECT_DIR:-$(git rev-parse --show-toplevel 2>/dev/null || echo .)}"
FOCUS_FILE="$ROOT/.claude/focus-story.md"
CACHE="$ROOT/.claude/.work-context-cache.md"

usage="usage: move-story.sh <issue-number> <todo|focus|done>"
n="${1:?$usage}"
col="${2:?$usage}"
case "$n" in *[!0-9]*) echo "move-story: '$n' is not an issue number — $usage" >&2; exit 2 ;; esac

case "$col" in
  todo) status="Todo" ;;
  focus) status="In Progress" ;;
  done) status="Done" ;;
  *) echo "move-story: unknown column '$col' — $usage" >&2; exit 2 ;;
esac

# ---- board ------------------------------------------------------------------
project_id=$(gh project view "$PROJECT" --owner "$OWNER" --format json --jq .id)
fields=$(gh project field-list "$PROJECT" --owner "$OWNER" --format json)
field_id=$(jq -r '.fields[] | select(.name=="Status") | .id' <<<"$fields")
option_id=$(jq -r --arg s "$status" \
  '.fields[] | select(.name=="Status") | .options[] | select(.name==$s) | .id' <<<"$fields")
if [ -z "$field_id" ] || [ -z "$option_id" ]; then
  echo "move-story: the board has no Status option named '$status'" >&2
  exit 1
fi

find_item() {
  gh project item-list "$PROJECT" --owner "$OWNER" --format json --limit 500 \
    | jq -r --argjson n "$n" '.items[] | select(.content.number==$n) | .id' | head -1
}
item_id=$(find_item)
if [ -z "$item_id" ]; then
  gh project item-add "$PROJECT" --owner "$OWNER" \
    --url "https://github.com/$OWNER/$REPO/issues/$n" >/dev/null
  item_id=$(find_item)
fi
[ -n "$item_id" ] || { echo "move-story: could not find or add a card for #$n" >&2; exit 1; }

gh project item-edit --id "$item_id" --project-id "$project_id" \
  --field-id "$field_id" --single-select-option-id "$option_id" >/dev/null

# ---- focus-story.md ---------------------------------------------------------
if [ -f "$FOCUS_FILE" ]; then
  sed -i -E "/^[[:space:]]*${n}[[:space:]]*\$/d" "$FOCUS_FILE"
fi
if [ "$col" = "focus" ]; then
  echo "$n" >>"$FOCUS_FILE"
fi

# ---- done closes the issue ----------------------------------------------------
closed=""
if [ "$col" = "done" ]; then
  if gh issue close "$n" --repo "$OWNER/$REPO" --reason completed >/dev/null 2>&1; then
    closed=" (issue closed)"
  else
    closed=" (issue already closed)"
  fi
fi

rm -f "$CACHE"
echo "move-story: #$n -> $status$closed"
