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

## bench_api.rs (~930 lines — more salvage here than the door suggests)
- **L1:** the DOOR is deleted; the contents scatter. The seeded workload
  constructors (`transitive_closure` with chain/dense/random shapes,
  `points_to`, `three_way_join`, `scan_filter`, `aggregation`) →
  `kyzo-core/benches/` as permanent instrumentation (benches may see
  internals; the test ontology already houses them). The #74 attribution
  probes (`parse_put_*`, `encode_only`, `probe_only_not_found`,
  `bare_fjall_put_batches`) → the benches of the zones they each isolate
  (model/parse, session/admit, store). `Workload::collect`'s
  iterator-vs-batched byte-equality claim → the trials differential.
- **L2:** gold: the opaque-façade discipline itself ("no crate-internal
  type crosses the boundary") — the same law the sealed contract states,
  just implemented as a door instead of as benches-in-zone; and the
  hand-built `MagicProgram` construction, which skips parse entirely so
  the timed region is evaluation only. Condemned: `Backend::Mem` — it
  selects `SimStorage`, which is DST machinery bound for kyzo-trials;
  zone benches measure the real substrate. Also note `Segments::OFF` and
  the panic-on-fixed-rules closure: bench plumbing that dies with the door.

## fuzz_api.rs (92 lines)
- **L1:** deleted. Generative fuzzing drives the public surface and the
  model's parse tier from `kyzo-trials`.
- **L2:** the door's only content is exposure (parse-without-panic, plus
  the two msgpack-island decode targets from #62's hostile follow-up);
  the fuzzer's corpus and generators are trials-crate property. Its own
  doc confirms the pattern: the memcmp-codec target needed NO façade
  because that surface is public — so when model/parse is its own crate,
  every target reaches its subject publicly and the door has nothing
  left to expose. Carry the enumerated fuzz-target list into the trials
  fuzz driver so no target is silently dropped.

## lsp_api.rs (49 lines)
- **L1:** deleted as a DOOR — but its content is a real product surface,
  not lab equipment: validate-without-executing (full resolve of
  params/aggregations/fixed rules, returning the exact diagnostic
  `run_script` would raise). In the target, that capability IS
  `kyzo-model`'s parse tier consumed directly by `kyzo-lsp/translate.rs`;
  the "fully resolve fixed rules" half needs the rules registry, which is
  the one part model alone cannot answer — adjudicate where
  resolve-completeness lives when the wall goes up.
- **L2:** preserve the contract statement ("Ok means the script would
  run; Err carries the designed diagnostic, live on every keystroke") —
  it is kyzo-lsp's product definition, written here because parse lives
  inside the engine crate today.
