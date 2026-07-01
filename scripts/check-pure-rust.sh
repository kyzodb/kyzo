#!/usr/bin/env bash
# Pure-Rust gate: kyzo-core and kyzo-bin must carry NO C/C++-toolchain crate in their
# dependency tree (normal + build edges). This mechanically enforces the point of the
# fork (see REFACTOR.md §3). The six language bindings are intrinsically FFI and are
# NOT checked here; their FFI crates are what a binding is.
#
# Runnable locally: scripts/check-pure-rust.sh
set -euo pipefail
cd "$(dirname "$0")/.."

if [ ! -f Cargo.toml ]; then
  echo "pure-Rust gate: no Cargo workspace yet — armed but idle (first bite: Slice 1)"
  exit 0
fi

# Crates whose presence means a C/C++ compiler or the banned base backends got in.
BANNED='^(cc|cmake|cxx|cxx-build|bindgen|pkg-config|sqlite3-src|libsqlite3-sys|rusqlite|librocksdb-sys|rocksdb|cozorocks) '

hits=$(cargo tree -p kyzo -p kyzo-bin -e normal,build --prefix none 2>/dev/null | sort -u | grep -E "$BANNED" || true)

if [ -n "$hits" ]; then
  echo "FAIL pure-Rust gate: C/C++-toolchain crates found in the kyzo-core/kyzo-bin dependency tree:"
  echo "$hits"
  exit 1
fi

echo "pure-Rust gate: clean (kyzo-core + kyzo-bin dependency tree carries no C/C++-toolchain crate)"
