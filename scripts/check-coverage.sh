#!/usr/bin/env bash
# Coverage ratchet: workspace line coverage must not drop below the recorded baseline
# (ci/coverage-baseline.txt, first recorded at Slice 3 green). Until a baseline exists
# the gate is REPORT-ONLY and says so loudly — there is no honest threshold before
# working code exists. Requires cargo-llvm-cov (CI installs it).
#
# Runnable locally: scripts/check-coverage.sh [workspace-dir]
set -euo pipefail
cd "${1:-$(dirname "$0")/..}"

if [ ! -f Cargo.toml ]; then
  echo "coverage gate: no Cargo workspace yet — armed but idle (first bite: Slice 3)"
  exit 0
fi

if ! cargo llvm-cov --version >/dev/null 2>&1; then
  echo "FAIL coverage gate: cargo-llvm-cov is not installed (CI must install it; locally: cargo install cargo-llvm-cov)"
  exit 1
fi

pct=$(cargo llvm-cov --workspace --summary-only --json 2>/dev/null | jq -r '.data[0].totals.lines.percent')

if [ ! -f ci/coverage-baseline.txt ]; then
  echo "coverage gate: REPORT-ONLY — workspace line coverage is ${pct}%."
  echo "No baseline recorded yet; the baseline + ratchet activate at Slice 3 green (issue #15)."
  exit 0
fi

baseline=$(cat ci/coverage-baseline.txt)
ok=$(python3 -c "print(1 if float('${pct}') + 1e-9 >= float('${baseline}') else 0)")
if [ "$ok" != "1" ]; then
  echo "FAIL coverage gate: line coverage ${pct}% dropped below baseline ${baseline}%."
  exit 1
fi
echo "coverage gate: clean (${pct}% >= baseline ${baseline}%)"
