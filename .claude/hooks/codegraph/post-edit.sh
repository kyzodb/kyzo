#!/bin/sh
# PostToolUse (Edit|Write|MultiEdit|NotebookEdit): nudge the agent to reconcile the graph
# after source edits — at most once every 30 minutes, and only in projects that actually
# have a codegraph. Update is a typed diff (unchanged files are hash-gated), so the nudge
# asks for something cheap.

. "$(dirname "$0")/detect.sh"

codegraph_configured || exit 0

# Debounce on a marker keyed by session id (parsed from the hook event on stdin; falls
# back to the project path hash if absent). stdin must be drained either way.
# Operator preference for this project: reconcile on EVERY diff — no debounce.
# stdin (the hook event) must still be drained.
cat >/dev/null

cat <<'EOF'
{"hookSpecificOutput": {"hookEventName": "PostToolUse", "additionalContext": "Source files have changed since the codegraph was last reconciled. When this batch of edits is complete, run the codegraph_update tool (cheap typed diff — unchanged files cost nothing) and read the report: purity + direction first, then the diff shape, then claims added/superseded."}}
EOF
