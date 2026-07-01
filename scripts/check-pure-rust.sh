#!/usr/bin/env bash
# Pure-Rust gate: kyzo-core and kyzo-bin must carry NO C/C++-toolchain crate in their
# dependency tree (normal + build edges). This mechanically enforces the point of the
# fork (see REFACTOR.md §3). The six language bindings are intrinsically FFI and are
# NOT checked here; their FFI crates are what a binding is.
#
# Runnable locally: scripts/check-pure-rust.sh [workspace-dir]
# (the optional dir argument exists so bite-proof fixtures can be checked)
set -euo pipefail
cd "${1:-$(dirname "$0")/..}"

if [ ! -f Cargo.toml ]; then
  echo "pure-Rust gate: no Cargo workspace yet — armed but idle"
  exit 0
fi

# Crates whose presence means a C/C++ compiler or the banned base backends got in.
BANNED='^(cc|cmake|cxx|cxx-build|bindgen|pkg-config|sqlite3-src|libsqlite3-sys|rusqlite|librocksdb-sys|rocksdb|cozorocks) '

# Query each engine package separately: a cargo error must FAIL the gate (a
# silently-empty tree reads as "clean"), while a package that simply has not
# landed yet is reported and skipped.
trees=""
checked=""
for p in kyzo kyzo-bin; do
  if out=$(cargo tree -p "$p" -e normal,build --prefix none 2>&1); then
    trees="$trees$out
"
    checked="$checked $p"
  elif printf '%s' "$out" | grep -q "not found in workspace\|did not match any packages"; then
    echo "note: package '$p' not in the workspace yet — the gate covers it when it lands"
  else
    echo "FAIL pure-Rust gate: 'cargo tree -p $p' errored (an unreadable tree is not a clean tree):"
    printf '%s\n' "$out"
    exit 1
  fi
done

if [ -z "$checked" ]; then
  echo "FAIL pure-Rust gate: no engine package found in the workspace"
  exit 1
fi

hits=$(printf '%s' "$trees" | sort -u | grep -E "$BANNED" || true)

if [ -n "$hits" ]; then
  echo "FAIL pure-Rust gate: C/C++-toolchain crates found in the engine dependency tree:"
  echo "$hits"
  exit 1
fi

echo "pure-Rust gate: clean (checked:$checked — no C/C++-toolchain crate in the dependency tree)"
