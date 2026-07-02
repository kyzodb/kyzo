#!/usr/bin/env bash
# PreToolUse guard for the on-disk-format blast-radius zones (see .claude/rules/memcmp.md and storage.md).
# Reads the tool-call JSON from stdin, and if the edited file is in a guarded zone,
# emits additionalContext warning the agent. Silent (exit 0, no output) otherwise.
#
# Test locally:
#   echo '{"tool_input":{"file_path":"kyzo-core/src/data/memcmp.rs"}}' | .claude/hooks/blast-radius-guard.sh
set -euo pipefail

file=$(jq -r '.tool_input.file_path // ""')

msg=""
case "$file" in
  *kyzo-core/src/data/memcmp.rs | *kyzo-core/src/data/tuple.rs)
    msg="memcmp.rs/tuple.rs are the ON-DISK key format: encoded byte order MUST equal semantic value order, and the key layout (relation prefix, fixed-width validity tail) is part of it. Any change is a DB migration (see .claude/rules/memcmp.md) and needs a round-trip+ordering test plus a format-version bump discussion."
    ;;
  *kyzo-core/src/storage/*)
    msg="storage/** implements the Storage/ReadTx/WriteTx contract for the single pure-Rust fjall backend: ordered scans, SSI commit with typed conflicts, validity-in-key time travel, no C/C++, and species invariants held by TYPES (reader cannot write; commit consumes). Never move an invariant down the enforcement ladder (see .claude/rules/storage.md)."
    ;;
  *)
    exit 0
    ;;
esac

jq -cn --arg m "$msg" '{hookSpecificOutput:{hookEventName:"PreToolUse",additionalContext:$m}}'
