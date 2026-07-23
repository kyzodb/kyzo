#!/usr/bin/env bash
# sessionStart: ensure durable bus monitor is running (never advances arm).
set -u
ROOT="${CURSOR_PROJECT_DIR:-${CLAUDE_PROJECT_DIR:-}}"
if [ -z "$ROOT" ]; then
  ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
fi
KYZO="$ROOT/.kyzo"
MONITOR="$KYZO/bus_monitor.py"
PIDFILE="$KYZO/bus-monitor.pid"
LOG="$KYZO/bus_monitor.log"

alive=0
if [ -f "$PIDFILE" ]; then
  pid="$(tr -d '[:space:]' <"$PIDFILE" || true)"
  if [ -n "${pid:-}" ] && kill -0 "$pid" 2>/dev/null; then
    alive=1
  fi
fi
if [ "$alive" -eq 0 ]; then
  if pgrep -f "$MONITOR" >/dev/null 2>&1; then
    alive=1
  fi
fi
if [ "$alive" -eq 0 ] && [ -f "$MONITOR" ]; then
  setsid /usr/bin/python3 "$MONITOR" >>"$LOG" 2>&1 </dev/null &
fi

# Fire-and-forget; optional context for the agent.
ts="$(date -Iseconds 2>/dev/null || date)"
echo "$ts sessionStart monitor_alive=$alive" >>"$KYZO/hooks-run.log" 2>/dev/null || true
printf '%s\n' '{"additional_context":"Agent bus monitor armed. Unread Claude→Cursor mail is latched by the stop hook; consume with: python3 .kyzo/mailbox.py read"}'
exit 0

