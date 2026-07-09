---
paths:
  - "kyzo-core/src/engines/hnsw_filter_harness.rs"
  - "kyzo-core/src/engines/gazetteer_hostile.rs"
  - "kyzo-core/src/engines/sparse_hostile.rs"
  - "kyzo-core/src/runtime/db_battery.rs"
---

# Absorbed — scaffolding and batteries dissolving into their proper homes

Standalone battery/harness files are structure masquerading as siblings of
the code they test; the target test ontology puts internal adversarial
tests in the module's test submodule, beside what they attack.

Entries below are census-verified: each file's construct inventory was
enumerated to closure before its verdicts landed.

## engines/sparse_hostile.rs (392 lines; inventory: module doc (hostile
review battery, NOT part of the reviewed module; independently-written
references), `#![cfg(test)]`, fixture plumbing (col/input_handle/
base_meta/`Fixture`/`Doc`/setup/params/`run_bits` projecting EXACT f32
bit patterns), and eight adversarial tests: query argument-order
irrelevance to score bits (exercises the admission sort the module's own
`summation_order_is_pinned` under-pins by feeding a pre-sorted query);
insertion-order irrelevance (postings in memcmp order make summation
insertion-independent); the independent f64 dot-product reference on
exact-representable weights; large tie-set top-k survivors pinned
(ascending-key tiebreak + truncation); denormal/huge weights finite and
deterministic (+inf a lawful score, NaN never); the reviewer-added
`sparse_total_docs` coverage (rows, not postings; empty base = 0); the
k=0 filter-path regression (the loop checked `len() >= k` AFTER pushing —
fixed check-before-push in BOTH this engine and the identical shape in
`fts.rs::fts_search`); -0.0 admitted and never a hit — closed)
- **L1:** absorbed into the sparse projection's test submodule at its
  target seat (`project/sparse/`), as a NAMED hostile-review section
  beside the module tests. The battery's independence lives in its
  independently-derived references (the f64 oracle, hand-computed bit
  patterns), not in file separation — rule #18's
  goldens-independently-derived survives the merge.
- **L2:** everything crosses; nothing is condemned. Keep loud: the
  exact-BIT score determinism standard (byte-identity, not tolerance);
  the pattern of hostile tests targeting exactly what the module's own
  tests under-pin; the k=0 regression pin, which must ALSO exist beside
  fts_search when the FTS battery lands (the fix was shared; the pin
  must be too — verify at fts.rs's census). Fixture plumbing follows the
  battery; where the target grows a shared projection-test rig, it
  dissolves there instead of duplicating per engine.

## engines/gazetteer_hostile.rs (516 lines; inventory: module doc
(independently-written leftmost-longest reference STRUCTURALLY different
from the module's own oracle: candidate enumeration + greedy resumption
vs the module's greedy scan), `#![cfg(test)]`, fixture plumbing
(input_handle/compile/view/pairs), `ref_tag` (the independent
reference), `assert_agree` (three assertions per case: engine ==
reference, every span boundary-truthful and slicing back to its surface,
determinism on re-tag), and the battery: Turkish dotted/dotless i (ASCII
folding must not reach into multibyte chars), ligatures + combining
marks adjacent to ASCII, three-way nesting and prefix/suffix overlaps,
whole-document surface + single-char carpets, 4000-case seeded xorshift
fuzz in both modes, the mutation differential (a `to_ascii_lowercase →
to_lowercase` mutant of the compiler diverges on İNDEX — a test that
kills a specific mutant), two-compiles agreement on adversarial docs,
and two `#[ignore]`d heavyweight probes (the law-5 corrupt-dictionary
sweep incl. a 2 MiB surface, and the reviewer's phase-timing scaling
probe noting the sweep once exceeded an 1800 s suite cap) — closed)
- **L1:** absorbed into the gazetteer projection's test submodule at its
  proposed seat (`project/gazetteer.rs`, NEW-SEAT — see the migrated
  entry), as a named hostile-review section. Independence lives in the
  structurally-different reference, which survives the merge.
- **L2:** everything crosses. Keep loud: TWO structurally different
  oracles for one engine (the module's greedy scan and this
  enumeration+resumption reference — agreement between three
  independent derivations is the strongest law here); the
  mutation-differential pattern (a test justified by the exact mutant it
  kills); the two `#[ignore]`s are pre-existing rule-#11 ledger items —
  on migration the corrupt sweep belongs in the trials/proving-ground
  lane and the scaling probe in the bench lane, not as ignored unit
  tests.
