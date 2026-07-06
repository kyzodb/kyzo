#!/usr/bin/env bash
# Unsafe-code gate for the pure-Rust engine (kyzo-core + kyzo-bin). The
# language bindings are exempt: unsafe FFI is what a binding is (see
# .claude/rules/ffi-bindings.md).
#
# kyzo-bin carries zero exceptions: #![forbid(unsafe_code)] makes any
# unsafe block there a COMPILE ERROR, full stop.
#
# kyzo-core carries exactly ONE reviewed exception (story #119's
# `GermanStr`, a hand-built 16-byte value in `data/germanstr.rs` — the
# first unsafe zone in the engine, Miri-audited by that module's own
# tests). Its crate root therefore declares #![deny(unsafe_code)] rather
# than `forbid`: `deny` still makes an unattributed unsafe block a compile
# error everywhere in the crate, but (unlike `forbid`) it can be locally
# lifted by an explicit `#[allow(unsafe_code)]` — which is exactly the
# opt-in `data/germanstr.rs` uses, and the only one that may exist. This
# script is the ratchet that keeps that exception from spreading: it greps
# the whole crate for the `allow(unsafe_code)` attribute and fails unless
# it finds that attribute in that one file and nowhere else. (It does not
# grep for the bare word "unsafe" — that also matches ordinary prose
# comments describing what would be unsafe; the attribute grep does not.)
#
# Runnable locally: scripts/check-unsafe.sh [workspace-dir]
set -euo pipefail
cd "${1:-$(dirname "$0")/..}"

if [ ! -f Cargo.toml ]; then
  echo "unsafe gate: no Cargo workspace yet — armed but idle"
  exit 0
fi

checked=""

if [ -f kyzo-bin/src/main.rs ]; then
  if ! grep -q '#!\[forbid(unsafe_code)\]' kyzo-bin/src/main.rs; then
    echo "FAIL unsafe gate: kyzo-bin/src/main.rs does not declare #![forbid(unsafe_code)]."
    echo "kyzo-bin carries zero exceptions; removing the forbid is a reviewed decision, not an edit."
    exit 1
  fi
  checked="$checked kyzo-bin/src/main.rs"
fi

if [ -f kyzo-core/src/lib.rs ]; then
  if ! grep -q '#!\[deny(unsafe_code)\]' kyzo-core/src/lib.rs; then
    echo "FAIL unsafe gate: kyzo-core/src/lib.rs does not declare #![deny(unsafe_code)]."
    echo "kyzo-core's one reviewed exception (data::germanstr) needs a liftable deny, not forbid; changing this is a reviewed decision, not an edit."
    exit 1
  fi

  # Anchored to (optional leading whitespace then) the attribute itself, so
  # this matches only a real `#![allow(unsafe_code)]` / `#[allow(unsafe_code)]`
  # line — never a comment or doc line that merely mentions the attribute
  # in prose (this file's own header does, further up).
  attr_pattern='^[[:space:]]*#!?\[allow\(unsafe_code\)\]'
  allowed_file="kyzo-core/src/data/germanstr.rs"
  if [ ! -f "$allowed_file" ] || ! grep -qE "$attr_pattern" "$allowed_file"; then
    echo "FAIL unsafe gate: $allowed_file does not declare its own #![allow(unsafe_code)]."
    echo "The one reviewed unsafe zone must opt in explicitly at its own module root."
    exit 1
  fi

  # The ratchet: the allow(unsafe_code) attribute must exist ONLY in the
  # one reviewed file. Widening it to a second file is exactly the "spread"
  # this gate exists to catch.
  offenders=$(grep -rlE --include='*.rs' "$attr_pattern" kyzo-core/src | grep -v -F "$allowed_file" || true)
  if [ -n "$offenders" ]; then
    echo "FAIL unsafe gate: allow(unsafe_code) found outside the one reviewed exception ($allowed_file):"
    echo "$offenders"
    exit 1
  fi

  checked="$checked kyzo-core/src/lib.rs"
fi

if [ -z "$checked" ]; then
  echo "unsafe gate: workspace exists but no engine crate roots yet — armed but idle"
  exit 0
fi

echo "unsafe gate: clean (kyzo-bin forbids unsafe entirely; kyzo-core denies it everywhere except the one reviewed exception:$checked)"
