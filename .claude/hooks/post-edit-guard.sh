#!/usr/bin/env bash
# PostToolUse(Edit|Write) hook: deterministic unsafe-policy enforcement.
# Blocks real `unsafe` (or allow(unsafe_code)) in first-party code and the
# removal of the forbid declaration. Silent when nothing matches.
set -euo pipefail

root="${CLAUDE_PROJECT_DIR:-$(git rev-parse --show-toplevel 2>/dev/null || echo .)}"
file=$(jq -r '.tool_input.file_path // ""')
[ -n "$file" ] || exit 0

abs="$file"
[ -f "$abs" ] || abs="$root/$file"
[ -f "$abs" ] || exit 0

warn() {
  jq -cn --arg m "$1" '{hookSpecificOutput:{hookEventName:"PostToolUse",additionalContext:$m}}'
  exit 0
}
block() {
  jq -cn --arg r "$1" '{decision:"block",reason:$r}'
  exit 0
}

# Real `unsafe` block / fn / impl (not prose), or a lint-lowering, anywhere in
# first-party forbid-governed code.
if printf '%s' "$file" | grep -Eq 'kyzo-[a-z-]+/src/'; then
  if grep -Eq '^[[:space:]]*(unsafe[[:space:]]*\{|unsafe[[:space:]]+fn|unsafe[[:space:]]+impl)' "$abs" \
     || grep -Eq '^[[:space:]]*#!?\[allow\(unsafe_code\)\]' "$abs"; then
    block "This introduces unsafe (or allow(unsafe_code)) into forbid-governed first-party code. Remove it, or open the deliberate narrowest-scope lint-lowering with a full safety case. Do not proceed with unsafe present."
  fi
fi

case "$file" in
  *crates/kyzo-core/src/lib.rs)
    if ! grep -q '#!\[forbid(unsafe_code)\]' "$abs"; then
      block "crates/kyzo-core/src/lib.rs no longer declares #![forbid(unsafe_code)]. Restore it — removing forbid is an in-story reviewed decision with a safety case, not an edit."
    fi
    if grep -Eqi 'germanstr[^a-z]*unsafe|reviewed exception|Miri-audited exception' "$abs"; then
      block "lib.rs claims an unsafe exception that does not exist. First-party code is pure safe Rust; delete the phantom-exception language."
    fi
    ;;
  *scripts/check-unsafe.sh)
    warn "You edited the unsafe guard. Run it now: bash scripts/check-unsafe.sh — a guard that lies is itself a failure."
    ;;
esac

exit 0
