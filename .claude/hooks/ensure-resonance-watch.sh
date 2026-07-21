#!/usr/bin/env bash
# SessionStart hook: make sure the resonance file watcher is running.
set -u
SCRIPT="${CLAUDE_PROJECT_DIR:-.}/crates/xtask/resonance-watch.sh"
if ! pgrep -f "xtask/resonance-watch.sh" > /dev/null 2>&1; then
  nohup "$SCRIPT" > /dev/null 2>&1 &
  disown 2>/dev/null || true
fi
exit 0
