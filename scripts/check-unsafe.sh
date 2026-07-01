#!/usr/bin/env bash
# Unsafe-code gate for the pure-Rust engine (kyzo-core + kyzo-bin). The
# language bindings are exempt: unsafe FFI is what a binding is (see
# .claude/rules/ffi-bindings.md).
#
# The guarantee is compiler-backed: every engine crate root must declare
# #![forbid(unsafe_code)], which makes any unsafe block a COMPILE ERROR —
# strictly stronger than source scanning, and immune to the word "unsafe"
# appearing in comments or strings.
#
# Runnable locally: scripts/check-unsafe.sh [workspace-dir]
set -euo pipefail
cd "${1:-$(dirname "$0")/..}"

if [ ! -f Cargo.toml ]; then
  echo "unsafe gate: no Cargo workspace yet — armed but idle"
  exit 0
fi

checked=""
for root in kyzo-core/src/lib.rs kyzo-bin/src/main.rs; do
  [ -f "$root" ] || continue
  if ! grep -q '#!\[forbid(unsafe_code)\]' "$root"; then
    echo "FAIL unsafe gate: $root does not declare #![forbid(unsafe_code)]."
    echo "The engine is 100% safe Rust by compiler guarantee; removing the forbid is a reviewed decision, not an edit."
    exit 1
  fi
  checked="$checked $root"
done

if [ -z "$checked" ]; then
  echo "unsafe gate: workspace exists but no engine crate roots yet — armed but idle"
  exit 0
fi

echo "unsafe gate: clean (#![forbid(unsafe_code)] enforced in:$checked)"
