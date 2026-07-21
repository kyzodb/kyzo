#!/usr/bin/env bash
# resonance-watch.sh — local file monitor for the combined GATE.
#
# Watches the source tree; on any relevant change, debounces ~2s and runs the
# combined gate in the kyzo-dev container, writing the verdict atomically to
# crates/xtask/resonance.log with a parsable header:
#   line 1: "GATE: PASS"  or  "GATE: FAIL resonance:<checks>"  or  "GATE: FAIL fast-tests"
#   body:   the failing detail (violations, or the failing test tail).
#
# Two stages, cheap-first:
#   1. resonance gate (~9s). If red, that's the verdict — don't spend minutes on tests.
#   2. fast lib tests: kyzo-model + kyzo --lib (debug). A failing unit test slams
#      the same way a red resonance gate does (the stop hook blocks on either).
# The heavy trials/crash/determinism campaigns are NOT here — they stay in CI's
# main/PR tier. The 12 #[ignore]'d slow tests auto-skip.
#
# A lock dir (resonance.log.lock) exists while a run is pending/active; a dirty
# flag set by mid-run edits triggers one more run, so the verdict always
# converges to the latest tree.
set -u

REPO="$(cd "$(dirname "$0")/../.." && pwd)"
LOG="$REPO/crates/xtask/resonance.log"
LOCK="$LOG.lock"
DIRTY="$LOG.dirty"
DEBOUNCE_SECS=2

run_gate_until_clean() {
  while :; do
    rm -f "$DIRTY"
    local tmp="$LOG.tmp"

    # Stage 1 — resonance (cheap). Gate the expensive tests on it.
    local raw rc out
    raw="$(cd "$REPO" && docker compose run --rm kyzo-dev \
      cargo run -p xtask --quiet -- resonance 2>&1)"
    rc=$?
    out="$(printf '%s\n' "$raw" | grep -v '^ Container kyzo-kyzo-dev-run-')"
    if [ "$rc" -ne 0 ]; then
      local checks
      checks="$(printf '%s\n' "$out" \
        | sed -n 's/^FAIL: resonance gate found violations in: //p' | tail -1)"
      printf 'GATE: FAIL resonance:%s\n%s\n' "${checks:-unknown}" "$out" > "$tmp"
      mv -f "$tmp" "$LOG"
      [ -e "$DIRTY" ] || break
      continue
    fi

    # Stage 2 — fast lib tests. A red test slams like a red gate.
    local traw trc tout
    traw="$(cd "$REPO" && docker compose run --rm kyzo-dev \
      cargo test -p kyzo-model -p kyzo --lib 2>&1)"
    trc=$?
    tout="$(printf '%s\n' "$traw" | grep -v '^ Container kyzo-kyzo-dev-run-' \
      | grep -E 'test result|error\[|^error:|panicked|FAILED|failures:|^ *[a-z_:]+::[a-z].* \.\.\. FAILED' | tail -40)"
    if [ "$trc" -ne 0 ]; then
      printf 'GATE: FAIL fast-tests\n%s\n' "$tout" > "$tmp"
    else
      printf 'GATE: PASS\n%s\n' "$tout" > "$tmp"
    fi
    mv -f "$tmp" "$LOG"
    [ -e "$DIRTY" ] || break
  done
}

relevant() {
  case "$1" in
    */target/*) return 1 ;;
    *.rs) return 0 ;;
    */agreements.toml|*/resonance-allow.toml) return 0 ;;
    */unchecked-arith-baseline.json|*/decode-surfaces.toml) return 0 ;;
    *) return 1 ;;
  esac
}

# Fresh verdict at startup so the log always exists.
if mkdir "$LOCK" 2>/dev/null; then
  ( trap 'rmdir "$LOCK" 2>/dev/null' EXIT; run_gate_until_clean ) &
fi

inotifywait -m -r -q \
  -e close_write -e moved_to -e create -e delete \
  --format '%w%f' \
  "$REPO/crates" "$REPO/resonance-allow.toml" \
| while IFS= read -r f; do
    relevant "$f" || continue
    touch "$DIRTY"
    if mkdir "$LOCK" 2>/dev/null; then
      (
        trap 'rmdir "$LOCK" 2>/dev/null' EXIT
        sleep "$DEBOUNCE_SECS"
        run_gate_until_clean
      ) &
    fi
  done
