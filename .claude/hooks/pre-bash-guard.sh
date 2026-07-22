#!/usr/bin/env bash
# PreToolUse(Bash) hook: nudge toward container-only nextest runs. Non-blocking
# by design — warns, never denies. A blunt substring match on raw command text
# is a bad tool for a hard wall (false positives on any command that merely
# quotes the pattern); its only real value is an instant, in-the-moment nudge
# against an honest, rushed native-invocation slip.
set -euo pipefail

cmd=$(jq -r '.tool_input.command // ""')
[ -n "$cmd" ] || exit 0

if ! printf '%s' "$cmd" | grep -q 'docker' \
   && printf '%s' "$cmd" | grep -Eq '(^|[^a-z])cargo[[:space:]]+nextest([[:space:]]|$)'; then
  jq -cn --arg m "Native cargo nextest is banned. Run it in the container: docker compose run --rm kyzo-dev cargo nextest run (environment.md)." \
    '{hookSpecificOutput:{hookEventName:"PreToolUse",additionalContext:$m}}'
fi

exit 0
