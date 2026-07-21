#!/usr/bin/env bash
# Story #30: the determinism campaign's outward-facing driver. The engine's
# own in-process campaigns (crates/kyzo-core/src/query/trials.rs,
# time_travel_trials.rs) already prove cross-thread determinism *inside one
# test binary*; this script proves the same claim across axes that can only
# be varied from OUTSIDE a single process: repeated runs, a real
# RAYON_NUM_THREADS sweep, and — the remaining gap issue #30 named — CPU
# architecture, by diffing digests recorded on two different runners.
#
# It drives crates/kyzo-trials/src/bin/determinism_digest.rs, a single-shot
# probe that runs a seeded mutation+time-travel+query workload through the
# public `Engine` API and prints one `COMBINED` hex digest (query answers,
# in returned row order — see the binary's module doc). Remade at the
# trials seat after museum cut 8ba3975.
#
# Usage: scripts/determinism-campaign.sh [digest-out] [digest-to-compare]
#   digest-out         where to write this run's agreed digest (default:
#                       determinism-digest.txt). This is the file CI uploads
#                       as the campaign's published artifact.
#   digest-to-compare  an earlier run's digest file (e.g. downloaded from a
#                       different architecture's CI job). If given, this
#                       run's digest must match it byte-for-byte or the
#                       script fails loudly — the cross-architecture check.
#
# Runnable locally: scripts/determinism-campaign.sh
set -euo pipefail
cd "$(dirname "$0")/.."

OUT="${1:-determinism-digest.txt}"
COMPARE="${2:-}"

probe() {
  RAYON_NUM_THREADS="$1" cargo run -p kyzo-trials --release --bin determinism_digest 2>&1
}

extract_digest() {
  # The probe's last line is `THREADS=<n> COMBINED=<hex>`; keep just the hex.
  grep -o 'COMBINED=[0-9a-f]*' | cut -d= -f2
}

echo "determinism campaign: building the probe once..."
cargo build -p kyzo-trials --release --bin determinism_digest >/dev/null

digests=()
labels=()

# Axis 1: thread count. A real rayon pool width each time — not a
# simulated/mocked count — because the batched RA path reads
# RAYON_NUM_THREADS when it builds its global pool on first use.
for threads in 1 2 4 8; do
  out=$(probe "$threads")
  d=$(echo "$out" | extract_digest)
  if [ -z "$d" ]; then
    echo "FAIL determinism campaign: probe produced no COMBINED digest at $threads threads:"
    echo "$out"
    exit 1
  fi
  digests+=("$d")
  labels+=("threads=$threads")
  echo "  threads=$threads  combined=$d"
done

# Axis 2: repeated runs, at a fixed (default) thread count — a fresh temp
# fjall store each time, same seeded workload.
for run in 1 2 3; do
  out=$(probe "default")
  d=$(echo "$out" | extract_digest)
  digests+=("$d")
  labels+=("repeat=$run")
  echo "  repeat=$run       combined=$d"
done

reference="${digests[0]}"
mismatches=0
for i in "${!digests[@]}"; do
  if [ "${digests[$i]}" != "$reference" ]; then
    echo "FAIL determinism campaign: ${labels[$i]} produced ${digests[$i]}, expected $reference"
    mismatches=$((mismatches + 1))
  fi
done
if [ "$mismatches" -gt 0 ]; then
  echo "FAIL determinism campaign: $mismatches of ${#digests[@]} runs disagreed — see above."
  exit 1
fi

echo "$reference" >"$OUT"
echo "determinism campaign: $((${#digests[@]})) runs agree on $reference (written to $OUT)"

if [ -n "$COMPARE" ]; then
  if [ ! -f "$COMPARE" ]; then
    echo "FAIL determinism campaign: nothing to compare at $COMPARE"
    exit 1
  fi
  other=$(cat "$COMPARE")
  if [ "$other" != "$reference" ]; then
    echo "FAIL determinism campaign: this architecture's digest ($reference) disagrees with $COMPARE ($other)."
    echo "This is a real cross-architecture nondeterminism finding, not a harness bug: FILE an engine issue, do not silence this check."
    exit 1
  fi
  echo "determinism campaign: matches the recorded digest in $COMPARE — cross-architecture check clean."
fi
