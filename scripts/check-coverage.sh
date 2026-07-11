#!/usr/bin/env bash
# Coverage ratchet: workspace line coverage must not drop below the recorded baseline
# (scripts/coverage-baseline.txt, recorded when the workspace first goes green). Until a baseline exists
# the gate is REPORT-ONLY and says so loudly — there is no honest threshold before
# working code exists. Requires cargo-llvm-cov (CI installs it).
#
# Runnable locally: scripts/check-coverage.sh [workspace-dir]
set -euo pipefail
cd "${1:-$(dirname "$0")/..}"

if [ ! -f Cargo.toml ]; then
  echo "coverage gate: no Cargo workspace yet — armed but idle"
  exit 0
fi

if ! command -v jq >/dev/null 2>&1; then
  echo "FAIL coverage gate: jq is not installed"
  exit 1
fi
if ! cargo llvm-cov --version >/dev/null 2>&1; then
  echo "FAIL coverage gate: cargo-llvm-cov is not installed (CI must install it; locally: cargo install cargo-llvm-cov)"
  exit 1
fi

pct=$(cargo llvm-cov --workspace --summary-only --json 2>/dev/null | jq -r '.data[0].totals.lines.percent')

if [ ! -f scripts/coverage-baseline.txt ]; then
  echo "coverage gate: REPORT-ONLY — workspace line coverage is ${pct}%."
  echo "No baseline recorded yet; the baseline + ratchet activate when the workspace first goes green."
  exit 0
fi

baseline=$(cat scripts/coverage-baseline.txt)
ok=$(awk -v p="$pct" -v b="$baseline" 'BEGIN { print (p + 1e-9 >= b) ? 1 : 0 }')
if [ "$ok" != "1" ]; then
  echo "FAIL coverage gate: line coverage ${pct}% dropped below baseline ${baseline}%."
  exit 1
fi
echo "coverage gate: clean (${pct}% >= baseline ${baseline}%)"
