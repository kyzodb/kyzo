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
# The C-carrying crypto/compression stacks (ring, aws-lc, openssl, native-tls, zstd,
# libz/lzma/bzip2 -sys) are named explicitly as defense in depth: each also pulls `cc`,
# but a named hit reads as "you picked the wrong TLS/codec stack — the pure choices are
# rustls+rustls-rustcrypto, miniz_oxide (flate2 rust_backend), and the brotli crate"
# instead of a bare toolchain violation. `-sys ` as a suffix is caught wholesale below.
BANNED='^(cc|cmake|cxx|cxx-build|bindgen|pkg-config|sqlite3-src|libsqlite3-sys|rusqlite|librocksdb-sys|rocksdb|cozorocks|ring|aws-lc-rs|aws-lc-sys|aws-lc-fips-sys|openssl|openssl-sys|openssl-src|native-tls|zstd|zstd-sys|zstd-safe|libz-sys|libz-ng-sys|lzma-sys|bzip2-sys) '
# Any *-sys crate is, by convention, a binding to a native library: none belongs in the
# engine tree. Caught as a class so new C bindings can't slip in under an unlisted name.
BANNED_SUFFIX='-sys v'
# The two -sys-by-name crates that are pure Rust: syscall/ABI *metadata* (constants and
# extern declarations), no C source, no cc/bindgen. Anything else must earn its own
# entry here with the same argument, in this comment.
PURE_SYS='^(linux-raw-sys|windows-sys) '

# Query each engine package separately: a cargo error must FAIL the gate (a
# silently-empty tree reads as "clean"), while a package that simply has not
# landed yet is reported and skipped.
#
# `--target=all` (issue #94): default `cargo tree` resolves ONLY the host
# target, so a dep gated behind `cfg(target_os="macos")`/`cfg(windows)` never
# appears on a Linux CI host — even though it's in Cargo.lock and DOES
# compile on those platforms. Concretely: `arrow` (via arrow-array's hard
# `chrono`+`clock` dependency) pulls `iana-time-zone` -> `core-foundation-sys`
# on macOS and `windows-core` on Windows, invisible to a host-only resolve.
# `--target=all` forces cross-platform resolution so every -sys/C-toolchain
# crate any shipped platform would pull is on the scanned surface, not just
# the CI runner's own.
# Warm the registry/dep cache FIRST. `cargo tree` below captures stderr
# (2>&1) to detect a "not found in workspace" error, but on a COLD cache
# (e.g. a fresh gate container) that same stderr carries "Updating index /
# Downloading ..." noise that pollutes the parsed tree and can false-match
# the banned-crate grep. Fetching first makes the tree parse deterministic.
cargo fetch --locked >/dev/null 2>&1 || cargo fetch >/dev/null 2>&1 || true

trees=""
checked=""
for p in kyzo kyzo-bin; do
  if out=$(cargo tree -p "$p" -e normal,build --target=all --prefix none 2>&1); then
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

hits=$(printf '%s' "$trees" | sort -u | grep -E -e "$BANNED" -e "$BANNED_SUFFIX" | grep -Ev "$PURE_SYS" || true)

if [ -n "$hits" ]; then
  echo "FAIL pure-Rust gate: C/C++-toolchain crates found in the engine dependency tree:"
  echo "$hits"
  exit 1
fi

echo "pure-Rust gate: clean (checked:$checked — no C/C++-toolchain crate in the dependency tree)"
