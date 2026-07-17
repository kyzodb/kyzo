#!/usr/bin/env bash
# PostToolUse(Edit|Write) hook: real-time unsafe-code tripwire.
# Blocks real `unsafe` (or allow(unsafe_code)) the instant it's typed, in the
# two forbid-governed engine crates (kyzo-core, kyzo-bin — language-binding
# crates are exempt, unsafe FFI is what a binding is; same ENGINE_CRATES set
# as crates/xtask/src/checks/unsafe_check.rs). Everything else about the
# unsafe policy — the forbid declaration staying present, and no doc/comment
# falsely claiming a reviewed unsafe exception — is checked once, at the
# gate, by unsafe_check.rs; it isn't duplicated here. Silent when nothing
# matches.
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

# Real `unsafe` block / fn / impl (not prose), or a lint-lowering, in the two
# forbid-governed engine crates only — same scope unsafe_check.rs enforces.
if printf '%s' "$file" | grep -Eq 'crates/(kyzo-core|kyzo-bin)/src/'; then
  if grep -Eq '^[[:space:]]*(unsafe[[:space:]]*\{|unsafe[[:space:]]+fn|unsafe[[:space:]]+impl)' "$abs" \
     || grep -Eq '^[[:space:]]*#!?\[allow\(unsafe_code\)\]' "$abs"; then
    block "This introduces unsafe (or allow(unsafe_code)) into forbid-governed first-party code. Remove it, or open the deliberate narrowest-scope lint-lowering with a full safety case. Do not proceed with unsafe present."
  fi
fi

case "$file" in
  *crates/xtask/src/checks/unsafe_check.rs)
    warn "You edited the unsafe guard. Run it now: docker compose run --rm kyzo-dev cargo xtask unsafe — a guard that lies is itself a failure."
    ;;
esac

exit 0
