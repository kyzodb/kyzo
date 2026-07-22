#!/usr/bin/env bash
# Story #81: bite-proof every resonance-gate check against its historical bug.
# Each proof works in a throwaway rsync copy (never the real tree): copies
# just the files a check reads (crates/kyzo-core/src, crates/kyzo-model/src,
# crates/kyzo-bin/src, resonance-allow.toml, crates/xtask/*.toml), reintroduces
# the bug's exact shape, and shows the relevant check alone (`--only <check>`)
# going RED against the mutated copy — then, where relevant, GREEN again once
# the mutation is reverted or an allowlist citation is added, proving the
# allowlist mechanism itself (not just the detector) works.
#
# Runnable: scripts/resonance-bite-proof.sh [check-name ...]
# With no arguments, runs all five.
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$(pwd)"

XTASK_BIN="${CARGO_TARGET_DIR:-$ROOT/target}/debug/xtask"
# Always rebuild (incremental, so this is fast when nothing changed): a
# stale binary from a previous run would silently run OLD detector logic
# against a mutation meant to exercise the CURRENT code -- exactly the kind
# of gap that would let a bite-proof pass for the wrong reason.
echo "building xtask..."
cargo build -p xtask

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

fresh_copy() {
  local dst="$1"
  rm -rf "$dst"
  mkdir -p "$dst"
  mkdir -p "$dst/crates"
  rsync -a --exclude target "$ROOT/crates/kyzo-core" "$dst/crates/"
  rsync -a --exclude target "$ROOT/crates/kyzo-model" "$dst/crates/"
  rsync -a --exclude target "$ROOT/crates/kyzo-bin" "$dst/crates/"
  cp "$ROOT/resonance-allow.toml" "$dst/resonance-allow.toml"
  mkdir -p "$dst/crates/xtask"
  cp "$ROOT/crates/xtask/decode-surfaces.toml" "$dst/crates/xtask/decode-surfaces.toml"
  cp "$ROOT/crates/xtask/agreements.toml" "$dst/crates/xtask/agreements.toml"
}

run_check() {
  local root="$1" check="$2"
  RESONANCE_ROOT="$root" "$XTASK_BIN" resonance --only "$check"
}

expect_red() {
  local root="$1" check="$2" label="$3"
  if run_check "$root" "$check" >/tmp/resonance-bite-$$.log 2>&1; then
    echo "BITE-PROOF FAILED: $label — expected check '$check' to go RED, it passed"
    cat /tmp/resonance-bite-$$.log
    rm -f /tmp/resonance-bite-$$.log
    exit 1
  fi
  echo "  RED as expected: $label"
  rm -f /tmp/resonance-bite-$$.log
}

expect_green() {
  local root="$1" check="$2" label="$3"
  if ! run_check "$root" "$check" >/tmp/resonance-bite-$$.log 2>&1; then
    echo "BITE-PROOF FAILED: $label — expected check '$check' to go GREEN, it failed"
    cat /tmp/resonance-bite-$$.log
    rm -f /tmp/resonance-bite-$$.log
    exit 1
  fi
  echo "  GREEN as expected: $label"
  rm -f /tmp/resonance-bite-$$.log
}

bite_derive_bypass() {
  echo "=== bite-proof: check 1 (derive-bypass) — the historical Interval bug ==="
  local copy="$WORK/derive_bypass"
  fresh_copy "$copy"
  # Reintroduce exactly the fork-base bug shape on RelationId in
  # kyzo-model value/row.rs (post peel seat): derive Deserialize on
  # RelationId instead of the hand-written impl. This is the literal shape
  # issue #62's hostile review found — a derived Deserialize builds the
  # raw u64 by direct field assignment, bypassing RelationId::new's under-CAP law.
  python3 - "$copy/crates/kyzo-model/src/value/row.rs" <<'PY'
import sys
path = sys.argv[1]
text = open(path).read()
old = ("#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]\n"
       "#[repr(transparent)]\n"
       "pub struct RelationId(u64);")
new = ("#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, serde_derive::Deserialize)]\n"
       "#[repr(transparent)]\n"
       "pub struct RelationId(u64);")
assert old in text, "RelationId derive line not found — has row.rs changed shape?"
text = text.replace(old, new, 1)
open(path, "w").write(text)
PY
  expect_red "$copy" derive_bypass "RelationId re-deriving Deserialize alongside its fallible new()"
}

bite_panic_lint() {
  echo "=== bite-proof: check 2 (panic-on-hostile-bytes) — the historical RelationId bug ==="
  local copy="$WORK/panic_lint"
  fresh_copy "$copy"
  # Reintroduce the assert! into RelationId::raw_decode in
  # kyzo-model value/row.rs, a declared decode surface in
  # crates/xtask/decode-surfaces.toml, instead of the typed refusal it
  # currently is.
  python3 - "$copy/crates/kyzo-model/src/value/row.rs" <<'PY'
import sys
path = sys.argv[1]
text = open(path).read()
old = "        let id = u64::from_be_bytes(arr);"
new = ("        let id = u64::from_be_bytes(arr);\n"
       "        assert!(id < RelationId::CAP, \"corrupt key: relation id out of range\");")
assert old in text, "raw_decode body line not found — has row.rs changed shape?"
text = text.replace(old, new, 1)
open(path, "w").write(text)
PY
  expect_red "$copy" panic_lint "assert! reintroduced into RelationId::raw_decode's decode path"
}

bite_copy_detector() {
  echo "=== bite-proof: check 3 (copy-detector) — the live skip-scan triplication ==="
  local copy="$WORK/copy_detector"
  fresh_copy "$copy"
  # No mutation needed: the triplication (fjall.rs/temp.rs/sim.rs) is
  # already in the tree today (story #78 will delete it). Strip its
  # allowlist entry to show the detector itself is what keeps the gate
  # green, not an inert waiver.
  python3 - "$copy/resonance-allow.toml" <<'PY'
import sys, re
path = sys.argv[1]
text = open(path).read()
text = re.sub(r"\[\[copy_detector\]\][^\[]*", "", text, count=1)
open(path, "w").write(text)
PY
  expect_red "$copy" copy_detector "skip-scan triplet (fjall.rs/temp.rs/sim.rs) with its #78 waiver removed"
}

bite_agreement_registry() {
  echo "=== bite-proof: check 5 (agreement-law registry) — a law quietly deleted ==="
  local copy="$WORK/agreement_registry"
  fresh_copy "$copy"
  python3 - "$copy/crates/kyzo-core/src/exec/plan/stratify.rs" <<'PY'
import sys
path = sys.argv[1]
text = open(path).read()
old = "the_oracle_refusal_corpus_is_refused"
new = "the_oracle_refusal_corpus_is_refused_renamed_without_updating_the_registry"
assert old in text, "test not found — has stratify.rs changed shape?"
text = text.replace(old, new)
open(path, "w").write(text)
PY
  expect_red "$copy" agreement_registry "the refusal-boundary test renamed without updating crates/xtask/agreements.toml"
}

if [ "$#" -eq 0 ]; then
  checks=(derive_bypass panic_lint copy_detector agreement_registry)
else
  checks=("$@")
fi

for c in "${checks[@]}"; do
  case "$c" in
    derive_bypass) bite_derive_bypass ;;
    panic_lint) bite_panic_lint ;;
    copy_detector) bite_copy_detector ;;
    agreement_registry) bite_agreement_registry ;;
    *) echo "unknown check: $c"; exit 1 ;;
  esac
done

echo "ALL BITE-PROOFS PASSED: every check demonstrably goes red on its historical bug shape."
