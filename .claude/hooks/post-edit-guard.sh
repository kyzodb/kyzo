#!/usr/bin/env bash
# PostToolUse(Edit|Write) hook: after a file is written, check it against the
# rule for its zone and inject a reminder (or block on the hardest violations).
# Silent when nothing matches.
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

# Real `unsafe` block / fn / impl (not prose) anywhere the value plane / engine
# roots forbid it.
if printf '%s' "$file" | grep -Eq 'kyzo-core/src/'; then
  if grep -Eq '^[[:space:]]*(unsafe[[:space:]]*\{|unsafe[[:space:]]+fn|unsafe[[:space:]]+impl)' "$abs" \
     || grep -Eq '^[[:space:]]*#!?\[allow\(unsafe_code\)\]' "$abs"; then
    block "This introduces unsafe (or allow(unsafe_code)) into forbid-governed first-party code (unsafe.md). Remove it, or open the deliberate narrowest-scope lint-lowering with a full safety case. Do not proceed with unsafe present."
  fi
fi

case "$file" in
  *kyzo-core/src/lib.rs)
    if ! grep -q '#!\[forbid(unsafe_code)\]' "$abs"; then
      block "kyzo-core/src/lib.rs no longer declares #![forbid(unsafe_code)] (unsafe.md). Restore it — removing forbid is an in-story reviewed decision with a safety case, not an edit."
    fi
    if grep -Eqi 'germanstr[^a-z]*unsafe|reviewed exception|Miri-audited exception' "$abs"; then
      block "lib.rs claims a GermanStr/unsafe exception that does not exist (unsafe.md). The value plane is pure safe Rust; delete the phantom-exception language."
    fi
    ;;
  *scripts/check-unsafe.sh)
    warn "You edited the unsafe guard. Run it now: bash scripts/check-unsafe.sh — a guard that lies is itself a failure (unsafe.md)."
    ;;
  *kyzo-core/src/data/value/*)
    if grep -Eq 'from_bytes|unchecked|from_raw' "$abs"; then
      warn "This value-plane file mentions from_bytes/unchecked/from_raw. No unchecked constructor, raw-code door, or forged wrapper may exist (value-plane.md). Confirm every code/CanonicalBytes/StampedCode/Minted path is admitted or mint-only, and that a compile-fail absence proof covers it."
    fi
    ;;
  *kyzo-core/src/storage/*|*kyzo-core/src/engines/*|*kyzo-core/src/runtime/relation.rs)
    if grep -Eq 'rmp_serde|Serializer::new' "$abs" && ! printf '%s' "$file" | grep -q 'tests'; then
      warn "This file uses rmp_serde outside tests. msgpack is a single sealed catalog door for CONFIG metadata only — no DataValue, no second value authority (storage-serialization.md). If this is a new persistence door, it must be ruled or it is forbidden."
    fi
    if grep -Eq 'DecodeError' "$abs" && printf '%s' "$file" | grep -q 'engines/'; then
      warn "An engine that surfaces a raw DecodeError across its boundary violates the index contract — codec failures become TYPED engine corruption (indexes.md)."
    fi
    ;;
  *tests/*|*tests.rs|*golden*|*fixture*)
    warn "You changed a test/golden/fixture. Never weaken to pass (tests-goldens.md): no 'any refusal' broadening, no deleted corruption-type, no golden copied from output. Classify any failure (old-false-behavior / impl-violation / deleted-vocabulary) and, for a golden, keep an independent-derivation note."
    ;;
esac

exit 0
