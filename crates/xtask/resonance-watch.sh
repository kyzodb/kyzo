#!/usr/bin/env bash
# resonance-watch.sh — file monitor for the resonance gate.
#
# Watches the source tree; on any relevant change, debounces ~2s, runs the
# gate in the kyzo-dev container, and atomically writes the report to
# crates/xtask/resonance.log with a parsable verdict header:
#   line 1: "RESONANCE: PASS"  or  "RESONANCE: FAIL <failing checks>"
#   body:   the gate's own output (violations only when red).
# A lock dir (resonance.log.lock) exists while a run is pending/active;
# a dirty flag set by mid-run edits triggers one more run, so the verdict
# always converges to the latest tree.
set -u

REPO="$(cd "$(dirname "$0")/../.." && pwd)"
LOG="$REPO/crates/xtask/resonance.log"
LOCK="$LOG.lock"
DIRTY="$LOG.dirty"
DEBOUNCE_SECS=2

run_gate_until_clean() {
  while :; do
    rm -f "$DIRTY"
    local out rc tmp checks
    local raw
    raw="$(cd "$REPO" && docker compose run --rm kyzo-dev \
      cargo run -p xtask --quiet -- resonance 2>&1)"
    rc=$?
    out="$(printf '%s\n' "$raw" | grep -v '^ Container kyzo-kyzo-dev-run-')"
    tmp="$LOG.tmp"
    if [ "$rc" -eq 0 ]; then
      printf 'RESONANCE: PASS\n%s\n' "$out" > "$tmp"
    else
      checks="$(printf '%s\n' "$out" \
        | sed -n 's/^FAIL: resonance gate found violations in: //p' | tail -1)"
      printf 'RESONANCE: FAIL %s\n%s\n' "${checks:-unknown}" "$out" > "$tmp"
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
