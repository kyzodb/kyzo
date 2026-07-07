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

# Shadow-story artifact guard (#140). The board is the work authority: a hidden
# scratchpad/tmp file must NOT stand in for a GitHub issue body. Catch a draft
# shaped like an issue body (name or a story-contract heading) written into a
# scratchpad/tmp path, and block it — edit the issue directly instead.
case "$file" in
  */scratchpad/*|/tmp/*)
    base=$(basename "$file")
    if printf '%s' "$base" | grep -Eiq '^(body|issue|story)[-_.].*\.md$|-(body|issue|story)\.md$' \
       || grep -Eqi '^##[[:space:]]*(hardest obligation|acceptance criteria|required invariant)|^failure mode:' "$abs" 2>/dev/null; then
      block "This is a hidden issue-body draft in a scratchpad/tmp path ($file). The board is the work authority (#140): local markdown must not stand in for a GitHub issue. Edit the issue directly (gh issue edit N --body-file - with transient stdin is fine), or post evidence via scripts/board-story-evidence. No shadow story artifacts as working truth."
    fi
    ;;
esac

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
    warn "You changed a test/golden/fixture. Never weaken to pass (tests-goldens.md): no 'any refusal' broadening, no deleted corruption-type, no golden copied from output. Classify any failure (old-false-behavior / impl-violation / deleted-vocabulary) and, for a golden, keep an independent-derivation note. If a healthy-path fixture uses raw storage writes that bypass the catalog/kernel, that is a construction violation (03-type-driven-construction.md), not a corruption test."
    ;;
esac

# Type-driven construction smell (content-based; 03-type-driven-construction.md).
# A WARN to classify, not a block — some are genuine boundaries.
#
# (a) A stringly domain KIND / format / string dispatch is forbidden OUTSIDE an
#     explicit decode/name boundary, anywhere in first-party code.
if printf '%s' "$file" | grep -Eq 'kyzo-[a-z-]+/src/' \
   && grep -Eq '(kind|format|ty|typ|type_name|index_kind|rel_kind|storage_kind|payload_kind)[[:space:]]*:[[:space:]]*(String|&str|Cow<)' "$abs"; then
  warn "This carries a domain KIND/format as a String (03-type-driven-construction.md). Forbidden outside an explicit decode/name boundary: make it a typed enum/newtype and dispatch by match. If it is a parse token / external name at the boundary, say so."
# (b) In the sensitive zones (storage, verifier, catalog, relation, index,
#     value), ANY Map/Set<String> must be CLASSIFIED — it is not auto-forbidden,
#     but string membership must not silently control storage meaning, relation/
#     index identity, verification dispatch, or authority.
elif printf '%s' "$file" | grep -Eq 'kyzo-core/src/(storage|engines|runtime/relation|data/value|query)' \
   && grep -Eq '(BTreeSet|HashSet|BTreeMap|HashMap)<[[:space:]]*String' "$abs"; then
  warn "A Map/Set<String> in storage/verifier/catalog/relation/index/value code MUST be classified (03-type-driven-construction.md): does its membership control verification, dispatch, storage meaning, relation/index identity, or authority? If yes, convert to a typed key/newtype. If it is only an external name / build-time catalog-name cross-reference resolved at a boundary, keep it and say so. Run scripts/smell-scan.sh."
fi

exit 0
