#!/bin/sh
# Shared detection: is a codegraph configured for this project? True when any CODEGRAPH_*
# variable is exported, or the project root (the hook's cwd) carries a `.codegraph` marker.
# Everything else in the hook layer keys off this one answer.

codegraph_configured() {
  [ -n "${CODEGRAPH_KYZO_URL:-}${CODEGRAPH_PROJECT:-}${CODEGRAPH_REPO:-}${CODEGRAPH_MAP:-}${CODEGRAPH_OVERLAY:-}${CODEGRAPH_SCOPE:-}" ] && return 0
  [ -e ".codegraph" ] && return 0
  return 1
}
