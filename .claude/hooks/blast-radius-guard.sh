#!/usr/bin/env bash
# PreToolUse guard for the on-disk-format blast-radius zones (see .claude/rules/value-plane.md and storage-serialization.md).
# Reads the tool-call JSON from stdin, and if the edited file is in a guarded zone,
# emits additionalContext warning the agent. Silent (exit 0, no output) otherwise.
#
# Test locally:
#   echo '{"tool_input":{"file_path":"kyzo-core/src/data/value/canonical.rs"}}' | .claude/hooks/blast-radius-guard.sh
set -euo pipefail

file=$(jq -r '.tool_input.file_path // ""')

msg=""
case "$file" in
  *kyzo-core/src/data/value/canonical.rs | *kyzo-core/src/data/value/tag.rs | *kyzo-core/src/data/value/number.rs | *kyzo-core/src/data/value/row.rs | *kyzo-core/src/data/bitemporal.rs)
    msg="canonical.rs/tag.rs/number.rs/row.rs/bitemporal.rs are the ON-DISK key+value format (value plane, FormatVersion 5: canonical byte format v1, tag-byte-first cross-type order, bitemporal two-slot key tail, value claim polarity): byte order MUST equal semantic value order (and DataValue::Ord must match the bytes), and any change to a RELEASED format is a DB migration (see .claude/rules/value-plane.md and storage-serialization.md) needing round-trip+ordering tests and a FormatVersion decision."
    ;;
  *kyzo-core/src/storage/*)
    msg="storage/** implements the Storage/ReadTx/WriteTx contract for the single pure-Rust fjall backend: ordered scans, SSI commit with typed conflicts, validity-in-key time travel, no C/C++, and species invariants held by TYPES (reader cannot write; commit consumes). Never move an invariant down the enforcement ladder (see .claude/rules/storage-serialization.md)."
    ;;
  *)
    exit 0
    ;;
esac

jq -cn --arg m "$msg" '{hookSpecificOutput:{hookEventName:"PreToolUse",additionalContext:$m}}'
