#!/usr/bin/env bash
# Stop hook: block stopping while the combined gate (resonance + fast lib tests)
# is dirty or mid-run.
set -u
LOG="${CLAUDE_PROJECT_DIR:-.}/crates/xtask/resonance.log"
LOCK="$LOG.lock"

if [ -d "$LOCK" ]; then
  exit 0
fi

[ -f "$LOG" ] || exit 0
head1="$(head -n 1 "$LOG")"

case "$head1" in
  "GATE: PASS"* | "RESONANCE: PASS"*) exit 0 ;;
  "GATE: FAIL fast-tests"*)
    {
      echo "GATE VIOLATION — STOP BLOCKED (fast lib tests are RED)."
      echo "$head1"
      echo ""
      echo "VIOLATION: Change FAIL to PASS in crates/xtask/resonance.log to acknowledge, then immediately sequence development work. This hook repeats every turn until resolved. Do not disable or modify it without explicit operator approval."

    } >&2
    exit 2
    ;;
  "GATE: FAIL"* | "RESONANCE: FAIL"*)
    {
      echo "GATE VIOLATION — STOP BLOCKED (resonance gate is RED)."
      echo "$head1"
      echo ""
      echo "Instruct the development team, over the real agent-message bus, to pull crates/xtask/resonance.log themselves and fix every violation in it now — URGENT. Never edit resonance.log's own verdict line to fabricate a pass, and never write to CLAUDE-AND-CURSOR.md (deleted, forbidden)."
    } >&2
    exit 2
    ;;
  *) exit 0 ;;
esac
