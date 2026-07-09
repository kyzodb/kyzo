---
paths:
  - "kyzo-core/src/engines/hnsw_filter_harness.rs"
  - "kyzo-core/src/engines/gazetteer_hostile.rs"
  - "kyzo-core/src/engines/sparse_hostile.rs"
  - "kyzo-core/src/runtime/db_battery.rs"
---

# Absorbed — scaffolding and batteries dissolving into their proper homes

Guidance grade: high-level review by smell/feel against the target purity
state. Standalone battery/harness files are structure masquerading as
siblings of the code they test; the target test ontology puts internal
adversarial tests in the module's test submodule, beside what they attack.

## engines/hnsw_filter_harness.rs (~1530 lines)
- **L1:** CORRECTED by the read — its Phase-2 subject (the filter-aware
  walk, Design V) has LANDED, and hnsw.rs's own doc now cites this harness
  as "the proof" of the `min(k, matches)` law. It is not dead scaffolding;
  it dissolves into `project/vector/`'s test submodule like its hostile
  siblings, instruments intact.
- **L2:** gold: the ground-truth oracle, selectivity-sweep generator,
  recall meter, and determinism harness — and the ADVERSARIAL INDEPENDENCE
  framing (the oracle re-implements the filter in native Rust and shares
  no code with the engine's bytecode eval or graph walk, "agreement is
  evidence, not tautology"). All of it survives the move; only the
  standalone-file ceremony and the phase framing die.

## engines/gazetteer_hostile.rs
- **L1:** dissolves into a `#[cfg(test)]` hostile submodule beside
  `project/text/`'s gazetteer engine.
- **L2:** the attack cases are GOLD — hostile-review capital paid for once;
  preserve every case through the move. What dies is the standalone-file
  ceremony and the "NOT part of the reviewed surface" framing (inside the
  module, that's automatic).

## engines/sparse_hostile.rs
- **L1:** dissolves into a `#[cfg(test)]` hostile submodule beside
  `project/sparse/`.
- **L2:** same law as the gazetteer battery: every attack case survives,
  the file does not.

## runtime/db_battery.rs
- **L1:** dissolves into the test submodule beside `session/db.rs`.
- **L2:** the adversarial session cases are gold and session-zone law
  (typed failures at the door) is their bar; keep the hostile-reviewer
  authorship spirit — these tests exist to distrust the door.
