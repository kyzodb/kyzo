#!/usr/bin/env bash
# resonance-watch.sh — file monitor for the bs-detector conduct gate.
#
# Watches the source tree; on any relevant change, debounces ~2s and runs
# the bs-detector in the kyzo-dev container. The detector binary itself is
# the writer of the verdict artifacts:
#   crates/xtask/resonance.log   (line 1: "RESONANCE: PASS" or
#                                 "RESONANCE: FAIL <failing checks>")
#   crates/xtask/bs-counts.txt   (one line: "name:N ... = TOTAL unconfessed")
# This script only writes the log when the detector could not run at all
# (compile error, container failure) — that is a FAIL too, never a stale
# green. A lock dir (resonance.log.lock) exists while a run is
# pending/active; a dirty flag set by mid-run edits triggers one more run,
# so the verdict always converges to the latest tree.
set -u

REPO="$(cd "$(dirname "$0")/../.." && pwd)"
LOG="$REPO/crates/xtask/resonance.log"
LOCK="$LOG.lock"
DIRTY="$LOG.dirty"
STAMP="$LOG.stamp"
DEBOUNCE_SECS=2

run_gate_until_clean() {
  while :; do
    rm -f "$DIRTY"
    touch "$STAMP"
    local raw rc
    raw="$(cd "$REPO" && docker compose run --rm kyzo-dev \
      cargo run --release --quiet -p bs-detector -- --root . 2>&1)"
    rc=$?
    # The binary writes the artifacts on every completed run (PASS or
    # FAIL). If the log is older than this run started, the run died
    # before verdict — report THAT as the verdict.
    if [ "$LOG" -ot "$STAMP" ]; then
      printf 'RESONANCE: FAIL detector-did-not-run (rc=%s)\n%s\n' "$rc" \
        "$(printf '%s\n' "$raw" | grep -v '^ Container kyzo-kyzo-dev-run-')" \
        > "$LOG.tmp"
      mv -f "$LOG.tmp" "$LOG"
    fi
    [ -e "$DIRTY" ] || break
  done
}

relevant() {
  case "$1" in
    */target/*) return 1 ;;
    *.rs) return 0 ;;
    */agreements.toml|*/waivers.toml|*/checks.toml) return 0 ;;
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
  "$REPO/crates" \
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
