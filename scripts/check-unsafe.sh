#!/usr/bin/env bash
# Unsafe-code ratchet for the pure-Rust engine (kyzo-core + kyzo-bin). The language
# bindings are exempt: unsafe FFI is what a binding is (see .claude/rules/ffi-bindings.md).
#
# Deterministic gate: the count of `unsafe` tokens in engine sources must not exceed
# the recorded baseline (ci/unsafe-baseline.txt, first recorded at Slice 1 with a
# reviewed commit). The count is crude (grep, so comments count too) but it compares
# like-to-like and needs no extra tooling; cargo-geiger runs in CI as an
# informational report on top of this gate, not instead of it.
#
# Runnable locally: scripts/check-unsafe.sh [workspace-dir]
set -euo pipefail
cd "${1:-$(dirname "$0")/..}"

if [ ! -f Cargo.toml ]; then
  echo "unsafe gate: no Cargo workspace yet — armed but idle (first bite: Slice 1)"
  exit 0
fi

dirs=()
for d in kyzo-core/src kyzo-bin/src; do
  [ -d "$d" ] && dirs+=("$d")
done
if [ ${#dirs[@]} -eq 0 ]; then
  echo "unsafe gate: workspace exists but no engine sources yet — armed but idle"
  exit 0
fi

count=$(grep -r --include='*.rs' -c -E '\bunsafe\b' "${dirs[@]}" 2>/dev/null | awk -F: '{s+=$NF} END {print s+0}')

if [ ! -f ci/unsafe-baseline.txt ]; then
  echo "FAIL unsafe gate: engine sources exist but ci/unsafe-baseline.txt is missing."
  echo "Record the baseline (current count: $count) in a reviewed commit — owed at Slice 1 (issue #15)."
  exit 1
fi

baseline=$(cat ci/unsafe-baseline.txt)
if [ "$count" -gt "$baseline" ]; then
  echo "FAIL unsafe gate: 'unsafe' occurrences in the engine grew: $count > baseline $baseline."
  echo "Growth requires an unsafe-invariants review and a deliberate, reviewed baseline bump."
  exit 1
fi
echo "unsafe gate: clean ($count occurrences <= baseline $baseline; engine only, bindings exempt)"
