#!/bin/sh
# SessionStart: if this project has a codegraph configured, tell the agent once, up front.
# Deliberately a silent no-op everywhere else — the plugin must add zero noise to projects
# that don't use codegraph.
#
# "Configured" means any CODEGRAPH_* variable is set in the environment, or the project
# keeps a `.codegraph` marker file at its root (the hook runs with cwd = project dir).

. "$(dirname "$0")/detect.sh"

codegraph_configured || exit 0

cat <<'EOF'
{"hookSpecificOutput": {"hookEventName": "SessionStart", "additionalContext": "This project has a codegraph: the codebase is parsed into a KyzoDB graph (typed constructs, vectors, architecture-map placement, doctrine claims, purity history), served by the kyzo MCP server. Before grepping or walking directories to answer questions about the code, use the kyzo-codegraph-query-mindset skill — one query usually replaces the whole loop. After changing source files, run the codegraph_update tool so the graph and the purity number stay current. Never settle proposals or purge without explicit human instruction (see kyzo-codegraph-operator-loop)."}}
EOF
