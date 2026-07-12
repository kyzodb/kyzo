#!/usr/bin/env bash
#
# KyzoDB benchmark runner — standard workloads only.
#
# Runs the transitive-closure benchmark (the vanilla recursive-Datalog
# workload) over real published SNAP graphs and records the result as
# bench-results/<commit>.txt — ONE file per run, named by the commit measured.
# A "baseline" is just one of those files; compare two runs by diffing them:
#
#     scripts/run-bench.sh
#     diff bench-results/<older>.txt bench-results/<newer>.txt
#
# No invented data and no invented queries: the graphs are fetched from their
# canonical SNAP source (scripts/fetch-bench-data.sh) and the program is the
# textbook transitive closure (crates/kyzo-core/examples/bench_tc.rs).
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
mkdir -p bench-results

# Standard graphs, smallest first. Extra args override this list.
GRAPHS=(${@:-email-Eu-core p2p-Gnutella08 wiki-Vote})
VARIANT="${VARIANT:-count}"

bash scripts/fetch-bench-data.sh >&2

SHA=$(git rev-parse --short HEAD)
OUT="bench-results/${SHA}.txt"

ulimit -v 12582912 || true
cargo build -p kyzo --release --example bench_tc >&2
BIN=target/release/examples/bench_tc

{
  echo "# KyzoDB transitive-closure benchmark"
  echo "commit:  ${SHA}  $(git log -1 --format=%s | cut -c1-64)"
  echo "date:    $(git log -1 --format=%cs)"
  echo "machine: $(uname -smr) | $(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2 | xargs) | $(nproc) cores"
  echo "workload: transitive closure (canonical 2-rule Datalog) over SNAP graphs, variant=${VARIANT}"
  echo
  for g in "${GRAPHS[@]}"; do
    timeout 1800 "$BIN" "bench-data/${g}.txt" "$VARIANT" || echo "TC graph=${g} FAILED (timeout/OOM under 12GiB cap)"
  done
} | tee "$OUT"
echo "wrote $OUT" >&2
