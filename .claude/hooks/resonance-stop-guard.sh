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
      echo "Cut the failing-test detail below the header out of crates/xtask/resonance.log, flip line 1 to \"GATE: PASS\", and post that detail to the development team in CLAUDE-AND-CURSOR.md right now — direct message, NO story, NO board task. Tell them: URGENT — a unit test is failing, fix this now."
    } >&2
    exit 2
    ;;
  "GATE: FAIL"* | "RESONANCE: FAIL"*)
    {
      echo "GATE VIOLATION — STOP BLOCKED (resonance gate is RED)."
      echo "$head1"
      echo ""
      echo "Cut the report below the header out of crates/xtask/resonance.log, flip line 1 to \"GATE: PASS\", and post that report to the development team in CLAUDE-AND-CURSOR.md right now — direct message, NO story, NO board task. Tell them: URGENT — fix this now."
    } >&2
    exit 2
    ;;
  *) exit 0 ;;
esac
