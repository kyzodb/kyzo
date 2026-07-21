#!/usr/bin/env bash
# Stop hook: block stopping while the resonance log is dirty or mid-run.
set -u
LOG="${CLAUDE_PROJECT_DIR:-.}/crates/xtask/resonance.log"
LOCK="$LOG.lock"

if [ -d "$LOCK" ]; then
  echo "RESONANCE GATE IS MID-RUN. Wait for crates/xtask/resonance.log to get its verdict, then stop again." >&2
  exit 2
fi

[ -f "$LOG" ] || exit 0
head1="$(head -n 1 "$LOG")"

case "$head1" in
  "RESONANCE: PASS"*) exit 0 ;;
  "RESONANCE: FAIL"*)
    {
      echo "RESONANCE GATE VIOLATION — STOP BLOCKED."
      echo "$head1"
      echo ""
      echo "Cut the report below the header out of crates/xtask/resonance.log, flip line 1 to \"RESONANCE: PASS\", and post that report to the development team in CLAUDE-AND-CURSOR.md right now — direct message, NO story, NO board task. Tell them: URGENT — fix this now."
    } >&2
    exit 2
    ;;
  *) exit 0 ;;
esac
