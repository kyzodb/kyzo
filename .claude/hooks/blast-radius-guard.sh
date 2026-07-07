#!/usr/bin/env bash
# PreToolUse(Edit|Write) guard. Two jobs, in order:
#
#   1. WORK AUTHORITY (#140): the board is authoritative. First-party CODE may be
#      edited only while exactly one valid `active-story` issue is in flight (per
#      the generated .claude/active-story.md cache). Zero active or multiple
#      active or a contract-invalid active story => code edits are DENIED. Docs,
#      the generated cache, and story templates are always allowed.
#
#   2. ON-DISK-FORMAT BLAST RADIUS: for the value-plane / storage zones, inject a
#      warning about the format contract (see .claude/rules/value-plane.md and
#      storage-serialization.md).
#
# Reads the tool-call JSON from stdin. Silent (exit 0) when nothing applies.
#
# Test locally:
#   echo '{"tool_input":{"file_path":"kyzo-core/src/data/value/canonical.rs"}}' | .claude/hooks/blast-radius-guard.sh
set -euo pipefail

root="${CLAUDE_PROJECT_DIR:-$(git rev-parse --show-toplevel 2>/dev/null || echo .)}"
input=$(cat 2>/dev/null || echo '{}')
file=$(printf '%s' "$input" | jq -r '.tool_input.file_path // ""')
[ -n "$file" ] || exit 0

deny() {
  jq -cn --arg r "$1" \
    '{hookSpecificOutput:{hookEventName:"PreToolUse",permissionDecision:"deny",permissionDecisionReason:$r}}'
  exit 0
}
warn() {
  jq -cn --arg m "$1" '{hookSpecificOutput:{hookEventName:"PreToolUse",additionalContext:$m}}'
  exit 0
}

# --- 1. Work-authority gate -------------------------------------------------
# Classify the target: is it first-party CODE (needs an active story) or a
# doc/generated/template file (always allowed)?
is_code=0
case "$file" in
  */.claude/active-story.md|*/.claude/story-templates/*|*/.claude/current-gate-report.md)
    is_code=0 ;;
  */.claude/hooks/*|*/.claude/rules/*|*/.claude/settings.json)
    is_code=1 ;;
  *kyzo*/src/*|*kyzo*/tests/*|*kyzo*/benches/*)
    is_code=1 ;;
  */scripts/*)
    is_code=1 ;;
  Cargo.toml|Cargo.lock|*/Cargo.toml|*/Cargo.lock|*justfile|*Dockerfile*|*docker-compose*)
    is_code=1 ;;
  *) is_code=0 ;;
esac

if [ "$is_code" -eq 1 ]; then
  story="$root/.claude/active-story.md"
  count=""; contract=""; active=""; sbranch=""
  if [ -f "$story" ]; then
    count=$(grep -m1 '^active_story_count:' "$story" | awk '{print $2}')
    contract=$(grep -m1 '^contract:' "$story" | awk '{print $2}')
    active=$(grep -m1 '^active_story:' "$story" | awk '{print $2}')
    sbranch=$(grep -m1 '^branch:' "$story" | awk '{print $2}')
  fi
  [ -n "$count" ] || count=0

  if [ "$count" = "0" ]; then
    deny "No active story (board carries no open \`active-story\` issue), so first-party code edits are blocked (#140). Editing: $file. The board is the work authority — label exactly one issue \`active-story\`, then re-sync (scripts/board-active-story-sync --force). Docs/board work may proceed."
  fi
  if [ "$count" != "1" ]; then
    deny "AMBIGUOUS active story (board carries $count \`active-story\` issues), so code edits are blocked (#140). Editing: $file. Resolve to exactly one \`active-story\` issue on the board, then re-sync."
  fi
  if [ "$contract" = "INVALID" ]; then
    deny "Active story $active fails its contract (story-contract-check). Code edits are blocked until the board issue body satisfies its kind's template (#140). Editing: $file. Fix the GitHub issue body, then re-sync."
  fi

  # Branch mismatch is a WARN (not a block): editing code off the story branch is
  # a smell, but detached HEAD / a deliberate main touch should not wedge.
  cur=$(git -C "$root" rev-parse --abbrev-ref HEAD 2>/dev/null || echo "?")
  glob="${sbranch:-}"
  if [ -n "$glob" ] && [ "$glob" != "-" ]; then
    # shellcheck disable=SC2254  # intentional glob match against branch pattern
    case "$cur" in
      $glob) : ;;
      *) warn "You are editing code on branch '$cur', but active story $active expects a '$glob' branch (#140). Confirm you are on the right story branch before landing." ;;
    esac
  fi
fi

# --- 2. On-disk-format blast-radius warnings (unchanged behavior) -----------
msg=""
case "$file" in
  *kyzo-core/src/data/value/canonical.rs | *kyzo-core/src/data/value/tag.rs | *kyzo-core/src/data/value/number.rs | *kyzo-core/src/data/value/row.rs | *kyzo-core/src/data/bitemporal.rs)
    msg="canonical.rs/tag.rs/number.rs/row.rs/bitemporal.rs are the ON-DISK key+value format (value plane, FormatVersion 5: canonical byte format v1, tag-byte-first cross-type order, bitemporal two-slot key tail, value claim polarity): byte order MUST equal semantic value order (and DataValue::Ord must match the bytes), and any change to a RELEASED format is a DB migration (see .claude/rules/value-plane.md and storage-serialization.md) needing round-trip+ordering tests and a FormatVersion decision."
    ;;
  *kyzo-core/src/storage/*)
    msg="storage/** implements the Storage/ReadTx/WriteTx contract for the single pure-Rust fjall backend: ordered scans, SSI commit with typed conflicts, validity-in-key time travel, no C/C++, and species invariants held by TYPES (reader cannot write; commit consumes). Never move an invariant down the enforcement ladder (see .claude/rules/storage-serialization.md)."
    ;;
  *)
    exit 0
    ;;
esac

jq -cn --arg m "$msg" '{hookSpecificOutput:{hookEventName:"PreToolUse",additionalContext:$m}}'
