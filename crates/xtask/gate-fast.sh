#!/usr/bin/env bash
# gate-fast.sh — one-shot combined bs-detector + fast lib-test run.
#
# Mirrors resonance-watch.sh's lock/log convention but covers the heavier
# cycle (isolated container, conduct gate, then the workspace lib tests).
# Always invoke this backgrounded; never run it synchronously in a terminal
# someone is watching.
#
# The bs-detector binary writes crates/xtask/resonance.log and
# bs-counts.txt itself; this script then overlays the combined GATE verdict
# the stop hook parses:
#   line 1: "GATE: PASS"  or  "GATE: FAIL fast-tests"  or  "GATE: FAIL <checks>"
#   line 2: "commit <sha>"
#   body:   the failing step's own output (only when red).
set -u

REPO="$(cd "$(dirname "$0")/../.." && pwd)"
LOG="$REPO/crates/xtask/resonance.log"
LOCK="$LOG.lock"

if ! mkdir "$LOCK" 2>/dev/null; then
  echo "gate-fast already running (lock held at $LOCK)" >&2
  exit 1
fi
trap 'rmdir "$LOCK" 2>/dev/null' EXIT

commit="$(cd "$REPO" && git rev-parse HEAD)"
short="$(cd "$REPO" && git rev-parse --short HEAD)"
tmp="$LOG.tmp"

printf 'GATE: FAIL running %s\nisolated snapshot + bs-detector + fast-tests in flight\n' \
  "$commit" > "$tmp"
mv -f "$tmp" "$LOG"

detector_out="$(cd "$REPO" && docker compose run --rm \
  --name "ci-snapshot-kyzo-dev-run-$short" kyzo-dev \
  cargo run --release --quiet -p bs-detector -- --root . 2>&1)"
detector_rc=$?

if [ "$detector_rc" -ne 0 ]; then
  checks="$(printf '%s\n' "$detector_out" \
    | sed -n 's/^RESONANCE: FAIL //p' | tail -1)"
  {
    printf 'GATE: FAIL %s\ncommit %s\n' "${checks:-bs-detector}" "$commit"
    printf '%s\n' "$detector_out" | grep -v '^ Container '
  } > "$tmp"
  mv -f "$tmp" "$LOG"
  exit 0
fi

test_out="$(cd "$REPO" && docker compose run --rm \
  --name "kyzo-lib-tests-$short" kyzo-dev \
  cargo nextest run --profile fast --workspace --lib 2>&1)"
test_rc=$?

if [ "$test_rc" -ne 0 ]; then
  {
    printf 'GATE: FAIL fast-tests\ncommit %s\n' "$commit"
    printf '%s\n' "$test_out"
  } > "$tmp"
else
  {
    printf 'GATE: PASS\ncommit %s\n' "$commit"
    printf '%s\n' "$test_out"
  } > "$tmp"
fi
mv -f "$tmp" "$LOG"
