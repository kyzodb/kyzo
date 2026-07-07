#!/usr/bin/env bash
#
# Copyright 2026, The KyzoDB Authors. MPL-2.0.
#
# smell-scan.sh — a CALLABLE, on-demand construction-discipline smell scan.
#
# NOT a gate, NOT a proof, NOT an always-on hook. It is a deliberately NOISY
# grep for the class of defect that lets a system LIE while it compiles:
# stringly/procedural/loose-collection code standing in for a Rust type. It
# exists because a verifier once shipped with a `String`/`BTreeSet` taxonomy
# instead of a typed `enum`, and no gate caught it.
#
# Usage:
#   scripts/smell-scan.sh                 # scan all first-party kyzo-*/src
#   scripts/smell-scan.sh <path...>       # scan specific paths
#   scripts/smell-scan.sh --strong        # only the single strongest combined scan
#
# EVERY hit MUST be classified (see the legend printed at the end) as one of:
#   real-violation | intentional-boundary | false-positive
# The scan finds candidates; YOU decide. Vendored deps (vendor/) and build
# output are excluded — this is about OUR construction discipline.

set -euo pipefail

RG=(rg -n --glob '*.rs' --glob '!vendor/**' --glob '!target/**')

# Default scope: first-party source. Override by passing paths.
scope=()
strong_only=0
for a in "$@"; do
  case "$a" in
    --strong) strong_only=1 ;;
    *) scope+=("$a") ;;
  esac
done
if [ "${#scope[@]}" -eq 0 ]; then
  while IFS= read -r d; do scope+=("$d"); done < <(ls -d kyzo-*/src 2>/dev/null)
  [ -d examples ] && scope+=(examples)
fi

# rg exits 1 when a pattern matches nothing; that is fine for a smell scan.
scan() { "${RG[@]}" "$@" "${scope[@]}" || true; }
section() { printf '\n\033[1m== %s ==\033[0m\n' "$1"; }

if [ "$strong_only" -eq 1 ]; then
  section "STRONGEST combined scan (the exact class that bit us)"
  scan -e 'kind\s*:\s*String' \
       -e 'BTreeSet<String>' -e 'HashSet<String>' \
       -e 'match .*as_str\(\)' \
       -e '==\s*"[A-Za-z0-9_:-]+"' \
       -e 'raw_put|put_raw|new_unchecked|from_raw' \
       -e 'unwrap_or\(i64::MAX\)' \
       -e 'assert!\(.+is_err\(\)\)'
  exit 0
fi

section "Stringly domain taxonomy (a String/set where an enum/newtype belongs)"
scan -e '\b(kind|format|ty|typ|type_name|rel_kind|storage_kind|payload_kind|index_kind)\s*:\s*(String|&str|Cow<)'
scan -e '\b(BTreeSet|HashSet|BTreeMap|HashMap)<\s*String\b'
scan -e '\b(match|if|else if).*(as_str\(\)|\.as_ref\(\)|\.to_string\(\))'
scan -e '==\s*"[A-Za-z0-9_:-]+"'
scan -e '\.contains\(\s*&?"[A-Za-z0-9_:-]+"\s*\)'

section "Raw storage / test bypass (fixtures that dodge catalog/kernel construction)"
scan -e '\b(raw_put|put_raw|put_kv|put_bytes|insert_raw|raw_insert|write_raw)\b'
scan -e '\.(put|insert)\([^)]*(encode|bytes|raw|key|val|value)'
scan -e 'relation[_ ]?id.*[=:]\s*[0-9]+'
scan -e '\b(Relation|RelationId|RelationHandle|Tuple|DataValue|EncodedKey).*\b(Default::default|dummy|fake|test_only|unchecked)\b'

section "Untyped IDs / primitive domain fields (a raw integer carrying identity)"
scan -e '\b(relation_id|rel_id|store_id|index_id|column_id|epoch|stamp|code|tag)\s*:\s*(u64|u32|usize|i64|i32|u16|u8)\b'
scan -e '\btype\s+(RelationId|RelId|StoreId|IndexId|ColumnId|Epoch|Stamp|Code|Tag)\s*=\s*(u64|u32|usize|i64|i32|u16|u8)\b'

section "Authority-bypass constructors (an unchecked door out of the authority module)"
scan -e '\b(new_unchecked|from_raw|from_bytes_unchecked|unchecked|assume_|dangerous|trusted|forge|mint_unchecked)\b'
scan -e '\bpub\s+fn\s+(new|from|decode|wrap|mint).*\b(raw|bytes|code|tag|stamp)\b'

section "Value-plane leaks (Ord-as-semantics, raw codes, tuple decode outside its lane)"
scan -e '\bDataValue::cmp\b|\bDataValue::partial_cmp\b|\.cmp\(&.*DataValue'
scan -e '\bStampedCode\b|\bRawCode\b|(^|[^_])\bCode\b'

section "Serialization authority (anything but canonical bytes / the ruled catalog door)"
scan -e '\brmp_serde\b|\bserde_json::to_vec\b|\bbincode\b|\bpostcard\b'
scan -e '\bDataValue\b.*\b(Serialize|Deserialize|rmp_serde|msgpack|bincode|serde_json)\b'

section "Sentinel / time leaks (i64::MAX as infinity through public semantics)"
scan -e '\bi64::MAX\b|\bi64::MIN\b|9223372036854775807'
scan -e '\bunwrap_or\s*\(\s*i64::MAX\s*\)|\bunwrap_or\s*\(\s*i64::MIN\s*\)'

section "Test weakening / proof dodges"
scan -e '#\[ignore\]|#\[allow\(dead_code\)\]|#\[allow\(unused|#\[allow\(warnings\)\]|TODO|FIXME|for now|good enough'
scan -e 'assert!\(.+is_err\(\)\)|assert_matches!\(.+Err\(_\)\)'

section "Unsafe / lint weakening"
scan -e '\bunsafe\b|allow\(unsafe_code\)|deny\(unsafe_code\)'

cat <<'LEGEND'

──────────────────────────────────────────────────────────────────────────────
CLASSIFY EVERY HIT — the scan finds candidates, you decide:

  ALLOWED (intentional-boundary / false-positive):
   - a decode boundary that IMMEDIATELY constructs a typed value
   - a corruption test EXPLICITLY named as corruption/bypass
   - a fixture that constructs through the real catalog/kernel
   - an internal primitive hidden behind a PRIVATE newtype/constructor

  VIOLATION (fix now, per rules/type-driven-construction.md):
   - string/set membership used as domain taxonomy
   - raw relation IDs in normal correctness tests
   - raw storage writes in non-corruption fixtures
   - unchecked constructors exposed OUTSIDE the authority module
   - value serialization outside canonical bytes / ruled catalog metadata
   - an exact error test weakened to "any error"
   - a sentinel time value leaking through public semantics
──────────────────────────────────────────────────────────────────────────────
LEGEND
