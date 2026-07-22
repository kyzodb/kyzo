#!/usr/bin/env bash
# Stop hook: block stopping while the combined gate (resonance + fast lib tests)
# is dirty or mid-run.
#
# OPERATOR CONTROL: crates/bs-detector/pester-hook.txt owns this hook.
# Comment (#) and blank lines there are documentation; the first remaining
# line is the switch — anything other than "on" disables the block entirely —
# and every remaining line after it is the violation message, printed
# verbatim. This script carries no copy of the message.
set -u

MSG="${CLAUDE_PROJECT_DIR:-.}/crates/bs-detector/pester-hook.txt"
[ -f "$MSG" ] || exit 0
switch="$(grep -v '^#' "$MSG" | grep -v '^[[:space:]]*$' | head -n 1)"
if [ "$switch" != "on" ]; then
  exit 0
fi

LOG="${CLAUDE_PROJECT_DIR:-.}/crates/xtask/resonance.log"
LOCK="$LOG.lock"

if [ -d "$LOCK" ]; then
  exit 0
fi

[ -f "$LOG" ] || exit 0
head1="$(head -n 1 "$LOG")"

case "$head1" in
  "GATE: PASS"* | "RESONANCE: PASS"*) exit 0 ;;
  "GATE: FAIL"* | "RESONANCE: FAIL"*)
    {
      echo "GATE VIOLATION — STOP BLOCKED (gate is RED)."
      echo "$head1"
      echo ""
      grep -v '^#' "$MSG" | grep -v '^[[:space:]]*$' | tail -n +2
    } >&2
    exit 2
    ;;
  *) exit 0 ;;
esac
