#!/usr/bin/env bash
# gate-fast.sh — one-shot combined resonance + fast lib-test run.
#
# Mirrors resonance-watch.sh's lock/log convention but covers the heavier
# cycle that was being run by hand (isolated container, resonance check,
# then `cargo test -p kyzo --lib`) — the exact thing that was showing up as
# raw build/test output dumped straight into a foreground chat session.
# Always invoke this backgrounded (`just gate` does this); never run it
# synchronously in a terminal someone is watching.
#
# Writes crates/xtask/resonance.log with the header resonance-stop-guard.sh
# already parses:
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

printf 'GATE: FAIL running %s\nisolated snapshot + resonance + fast-tests in flight\n' \
  "$commit" > "$tmp"
mv -f "$tmp" "$LOG"

resonance_out="$(cd "$REPO" && docker compose run --rm \
  --name "ci-snapshot-kyzo-dev-run-$short" kyzo-dev \
  cargo run -p xtask --quiet -- resonance 2>&1)"
resonance_rc=$?

if [ "$resonance_rc" -ne 0 ]; then
  checks="$(printf '%s\n' "$resonance_out" \
    | sed -n 's/^FAIL: resonance gate found violations in: //p' | tail -1)"
  {
    printf 'GATE: FAIL %s\ncommit %s\n' "${checks:-unknown}" "$commit"
    printf '%s\n' "$resonance_out"
  } > "$tmp"
  mv -f "$tmp" "$LOG"
  exit 0
fi

test_out="$(cd "$REPO" && docker compose run --rm \
  --name "kyzo-lib-tests-$short" kyzo-dev \
  cargo test --workspace --lib 2>&1)"
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
