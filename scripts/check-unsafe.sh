#!/usr/bin/env bash
# Unsafe-code gate for the pure-Rust engine (kyzo-core + kyzo-bin). The
# language bindings are exempt: unsafe FFI is what a binding is (see
# .claude/rules/ffi-bindings.md).
#
# BOTH engine crates carry ZERO exceptions: #![forbid(unsafe_code)] makes any
# unsafe block a COMPILE ERROR, full stop, and `forbid` (unlike `deny`)
# cannot be locally lifted by an `#[allow(unsafe_code)]`. The value plane —
# including GermanStr's 16-byte layout — is pure safe Rust; there is no
# reviewed exception and no reserved future unsafe zone. A later story that
# genuinely needs unsafe must lower the lint deliberately in that story, at
# the narrowest scope, with a full safety case — it does not get a phantom
# allowance today.
#
# This script enforces three things on the governed first-party surface:
#   1. each engine crate root declares #![forbid(unsafe_code)];
#   2. NO `allow(unsafe_code)` attribute appears anywhere in it;
#   3. the docs do not claim an unsafe exception that does not exist
#      (a lying guard doc is itself a failure).
#
# Runnable locally: scripts/check-unsafe.sh [workspace-dir]
set -euo pipefail
cd "${1:-$(dirname "$0")/..}"

if [ ! -f Cargo.toml ]; then
  echo "unsafe gate: no Cargo workspace yet — armed but idle"
  exit 0
fi

checked=""
# Real attribute lines only — not prose comments that mention the attribute.
allow_pattern='^[[:space:]]*#!?\[allow\(unsafe_code\)\]'

check_crate() {
  # $1 = crate root file, $2 = crate src dir
  local root="$1" src="$2"
  if [ ! -f "$root" ]; then
    return 0
  fi
  if ! grep -q '#!\[forbid(unsafe_code)\]' "$root"; then
    echo "FAIL unsafe gate: $root does not declare #![forbid(unsafe_code)]."
    echo "First-party engine code forbids unsafe with zero exceptions; removing forbid is a reviewed, in-story decision, not an edit."
    exit 1
  fi
  # Zero allow(unsafe_code) anywhere in the governed surface.
  local offenders
  offenders=$(grep -rlE --include='*.rs' "$allow_pattern" "$src" || true)
  if [ -n "$offenders" ]; then
    echo "FAIL unsafe gate: allow(unsafe_code) found in the forbid-governed surface ($src):"
    echo "$offenders"
    echo "There is no unsafe exception. A new one must be introduced deliberately in its own story with a full safety case."
    exit 1
  fi
  # The docs must not claim an exception that does not exist. A guard that
  # lies is worse than no guard.
  local liars
  liars=$(grep -rlniE 'germanstr[^a-z]*unsafe|unsafe[- ]exception|reviewed exception|Miri-audited exception' "$src" || true)
  if [ -n "$liars" ]; then
    echo "FAIL unsafe gate: $src claims an unsafe exception that does not exist:"
    echo "$liars"
    echo "The value plane is pure safe Rust. Delete the phantom exception language so the docs match the enforced rule."
    exit 1
  fi
  checked="$checked $root"
}

check_crate "kyzo-bin/src/main.rs" "kyzo-bin/src"
check_crate "kyzo-core/src/lib.rs" "kyzo-core/src"

if [ -z "$checked" ]; then
  echo "unsafe gate: workspace exists but no engine crate roots yet — armed but idle"
  exit 0
fi

echo "unsafe gate: clean — both engine crates forbid unsafe with zero exceptions:$checked"
