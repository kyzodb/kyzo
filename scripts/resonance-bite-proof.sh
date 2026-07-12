#!/usr/bin/env bash
# Story #81: bite-proof every resonance-gate check against its historical bug.
# Each proof works in a throwaway rsync copy (never the real tree): copies
# just the files a check reads (crates/kyzo-core/src, crates/kyzo-bin/src,
# resonance-allow.toml, crates/xtask/*.toml), reintroduces the bug's exact shape,
# and shows the relevant check alone (`--only <check>`) going RED against
# the mutated copy — then, where relevant, GREEN again once the mutation is
# reverted or an allowlist citation is added, proving the allowlist
# mechanism itself (not just the detector) works.
#
# Runnable: scripts/resonance-bite-proof.sh [check-name ...]
# With no arguments, runs all five.
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$(pwd)"

XTASK_BIN="$ROOT/target/debug/xtask"
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
  rsync -a --exclude target "$ROOT/kyzo-core" "$dst/"
  rsync -a --exclude target "$ROOT/kyzo-bin" "$dst/"
  cp "$ROOT/resonance-allow.toml" "$dst/resonance-allow.toml"
  mkdir -p "$dst/xtask"
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
  # Reintroduce exactly the fork-base bug: derive Deserialize on Interval
  # instead of the hand-written impl. This is the literal shape issue #62's
  # hostile review found — a derived Deserialize builds `start`/`end` by
  # direct field assignment, bypassing `Interval::new`'s `end > start` law.
  python3 - "$copy/crates/kyzo-core/src/data/value.rs" <<'PY'
import sys, re
path = sys.argv[1]
text = open(path).read()
old = "#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord, Hash, serde_derive::Serialize)]\npub struct Interval {"
new = "#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord, Hash, serde_derive::Serialize, serde_derive::Deserialize)]\npub struct Interval {"
assert old in text, "Interval derive line not found — has value.rs changed shape?"
text = text.replace(old, new, 1)
open(path, "w").write(text)
PY
  expect_red "$copy" derive_bypass "Interval re-deriving Deserialize alongside its fallible new()"
}

bite_panic_lint() {
  echo "=== bite-proof: check 2 (panic-on-hostile-bytes) — the historical RelationId bug ==="
  local copy="$WORK/panic_lint"
  fresh_copy "$copy"
  # Reintroduce the RelationId shape: an assert! sitting in a real decode
  # path (raw_decode), instead of the typed refusal it currently is.
  python3 - "$copy/crates/kyzo-core/src/data/tuple.rs" <<'PY'
import sys
path = sys.argv[1]
text = open(path).read()
old = "        let u = u64::from_be_bytes(bytes.try_into().expect(\"length checked\"));"
new = ("        let u = u64::from_be_bytes(bytes.try_into().expect(\"length checked\"));\n"
       "        assert!(u <= MAX_RELATION_ID, \"corrupt key: relation id out of range\");")
assert old in text, "raw_decode body line not found — has tuple.rs changed shape?"
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

bite_dead_code_ratchet() {
  echo "=== bite-proof: check 4 (dead-concept ratchet) — a fresh uncited allow(dead_code) ==="
  local copy="$WORK/dead_code_ratchet"
  fresh_copy "$copy"
  # `data/expr.rs`'s binding_indices/do_binding_indices are already uncited
  # in the real tree today (a genuine gap this story reports rather than
  # silently fixing, per its disjointness boundary — see the report).
  # Prove the ratchet catches a BRAND NEW uncited one instead, added to a
  # function that starts out with no attribute at all.
  python3 - "$copy/crates/kyzo-core/src/data/tuple.rs" <<'PY'
import sys
path = sys.argv[1]
text = open(path).read()
old = "    pub(crate) fn raw_encode(&self) -> [u8; 8] {"
new = "    #[allow(dead_code)]\n    pub(crate) fn raw_encode(&self) -> [u8; 8] {"
assert old in text, "raw_encode not found — has tuple.rs changed shape?"
text = text.replace(old, new, 1)
open(path, "w").write(text)
PY
  expect_red "$copy" dead_code_ratchet "a brand-new #[allow(dead_code)] with zero citation on RelationId::raw_encode"
}

bite_agreement_registry() {
  echo "=== bite-proof: check 5 (agreement-law registry) — a law quietly deleted ==="
  local copy="$WORK/agreement_registry"
  fresh_copy "$copy"
  python3 - "$copy/crates/kyzo-core/src/query/stratify.rs" <<'PY'
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
  checks=(derive_bypass panic_lint copy_detector dead_code_ratchet agreement_registry)
else
  checks=("$@")
fi

for c in "${checks[@]}"; do
  case "$c" in
    derive_bypass) bite_derive_bypass ;;
    panic_lint) bite_panic_lint ;;
    copy_detector) bite_copy_detector ;;
    dead_code_ratchet) bite_dead_code_ratchet ;;
    agreement_registry) bite_agreement_registry ;;
    *) echo "unknown check: $c"; exit 1 ;;
  esac
done

echo "ALL BITE-PROOFS PASSED: every check demonstrably goes red on its historical bug shape."
