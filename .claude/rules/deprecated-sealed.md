---
paths:
  - "kyzo-core/src/bench_api.rs"
  - "kyzo-core/src/fuzz_api.rs"
  - "kyzo-core/src/lsp_api.rs"
---

# Sealed — bespoke doors deleted outright; the opening was the defect

Guidance grade: high-level review by smell/feel against the target purity
state. These three files are private façades punched through the crate for
tooling. In the target state tooling speaks the sealed contract or the
judge crates; a bespoke door is contract debt. No successor files exist —
the CONSUMERS rewire.

## bench_api.rs
- **L1:** deleted. Benches become permanent instrumentation inside their
  zones behind the bench feature, or measurement contracts in the public
  proving ground.
- **L2:** nothing salvageable in the door itself; before deletion, confirm
  each bench it serves has a home (zone bench or proving-ground rig) so no
  measurement coverage silently dies.

## fuzz_api.rs
- **L1:** deleted. Generative fuzzing drives the public surface and the
  model's parse tier from `kyzo-trials`.
- **L2:** the door's only content is exposure; the fuzzer's corpus and
  generators are trials-crate property. Confirm the parse tier's public
  surface suffices — if the fuzzer needed a private hole, the surface was
  incomplete, and that gets fixed in the surface.

## lsp_api.rs
- **L1:** deleted. The language server consumes `kyzo-model`'s parse tier —
  the same lift and refusals users get.
- **L2:** nothing to salvage; the door exists only because parse lives
  inside the engine crate today. It dies naturally the moment the model
  crate exists.
