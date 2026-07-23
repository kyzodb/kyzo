#!/usr/bin/env bash
# PreToolUse(Edit|Write|MultiEdit) + PreToolUse(Bash) hook: waivers.toml is
# writable by the kyzo-waiver subagent ONLY. The waiver file is the single
# place a detector hit can be excepted; this guard makes that authority
# mechanical — the main session, every other subagent, and shell redirection
# are all denied. Enforcement lives here, not in agent prose.
set -euo pipefail

input=$(cat)
agent=$(printf '%s' "$input" | jq -r '.agent_type // "main"')
tool=$(printf '%s' "$input" | jq -r '.tool_name // ""')

deny() {
  jq -cn --arg m "$1" \
    '{hookSpecificOutput:{hookEventName:"PreToolUse",permissionDecision:"deny",permissionDecisionReason:$m}}'
  exit 0
}

write_shaped='>|(^|[^[:alnum:]_.-])(tee|mv|cp|rm|truncate|dd|python3?|perl|patch)([^[:alnum:]_.-]|$)|sed[[:space:]]+-i|awk[[:space:]]+-i|git[[:space:]]+(checkout|restore|apply|stash|reset|clean)'

# The standards skill is operator-written law: denied to EVERY agent,
# kyzo-waiver included. Checked before the agent exemption on purpose.
if [ "$tool" = "Bash" ]; then
  cmd=$(printf '%s' "$input" | jq -r '.tool_input.command // ""')
  if printf '%s' "$cmd" | grep -q 'kyzo-architecture-standards' \
     && printf '%s' "$cmd" | grep -Eq "$write_shaped"; then
    deny "kyzo-architecture-standards/SKILL.md is operator-written law. Propose the change to the operator in chat; do not edit it."
  fi
else
  path=$(printf '%s' "$input" | jq -r '.tool_input.file_path // .tool_input.notebook_path // ""')
  case "$path" in
    *kyzo-architecture-standards/SKILL.md)
      deny "kyzo-architecture-standards/SKILL.md is operator-written law. Propose the change to the operator in chat; do not edit it."
      ;;
  esac
fi

[ "$agent" = "kyzo-waiver" ] && exit 0

if [ "$tool" = "Bash" ]; then
  # Reads are legal for everyone; deny only commands that both name the file
  # and carry a write-shaped operation.
  if printf '%s' "$cmd" | grep -q 'waivers\.toml' \
     && printf '%s' "$cmd" | grep -Eq "$write_shaped"; then
    deny "waivers.toml is written only by the kyzo-waiver agent. Spawn it via the Agent tool with a waiver request carrying the verbatim attestation; it grants or refuses."
  fi
  exit 0
fi

case "$path" in
  *crates/bs-detector/waivers.toml|waivers.toml)
    deny "waivers.toml is written only by the kyzo-waiver agent. Spawn it via the Agent tool with a waiver request carrying the verbatim attestation; it grants or refuses."
    ;;
esac

exit 0
