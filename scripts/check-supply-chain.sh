#!/usr/bin/env bash
# Supply-chain gate: cargo vet. Idle before a workspace exists; once Cargo.toml lands,
# an UNINITIALIZED cargo vet is a hard failure — the Slice 1 activation debt is
# enforced by the gate itself, not remembered by anyone. Requires cargo-vet
# (CI installs it).
#
# Runnable locally: scripts/check-supply-chain.sh [workspace-dir]
set -euo pipefail
cd "${1:-$(dirname "$0")/..}"

if [ ! -f Cargo.toml ]; then
  echo "supply-chain gate: no Cargo workspace yet — armed but idle (first bite: Slice 1)"
  exit 0
fi

if [ ! -d supply-chain ]; then
  echo "FAIL supply-chain gate: workspace exists but cargo vet is not initialized."
  echo "Run 'cargo vet init' and audit/exempt the dependency tree — owed at Slice 1 (issue #15)."
  exit 1
fi

if ! command -v cargo-vet >/dev/null 2>&1; then
  echo "FAIL supply-chain gate: cargo-vet is not installed (CI must install it; locally: cargo install cargo-vet)"
  exit 1
fi

cargo vet check
echo "supply-chain gate: clean (cargo vet check passed)"
