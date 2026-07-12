---
paths:
  - "crates/kyzo-core/src/bench_api.rs"
  - "crates/kyzo-core/src/fuzz_api.rs"
  - "crates/kyzo-core/src/lsp_api.rs"
---

# Sealed — bespoke doors deleted outright; the opening was the defect

These files are private façades punched through the crate for tooling. In
the target state tooling speaks the sealed contract or the judge crates; a
bespoke door is contract debt. No successor files exist — the CONSUMERS
rewire.

Entries below are census-verified: each file's construct inventory was
enumerated to closure before its verdicts landed.

## bench_api.rs (932 lines; inventory: module doc, `Backend`,
span/symbol/program-builder plumbing (sp/sym/muggle/entry_symbol/v/col/
rule_atom/rel_atom/plain_rule/program_of), `Workload` + `BackendStore`
(label/run/collect/run_on/collect_on), `generous_budget`,
`immortal_lifetimes`, `SeedRelation`, `build`, `seed_backend`, `compile`,
`Graph` + `gen_edges`, five workload constructors (transitive_closure,
points_to, three_way_join, scan_filter, aggregation), and the #74
attribution section (synthetic_rows, put_literal_script,
PUT_PARAM_SCRIPT, param_pool_of, parse_put_literal, parse_put_param,
run_put_batches, bare_fjall_put_batches, encode_only,
probe_only_not_found) — closed)
- **L1:** the DOOR is deleted; the contents scatter by kind of truth. The
  seeded workload constructors + `Graph` shapes → `crates/kyzo-core/benches/`
  (permanent instrumentation; benches may see internals per the test
  ontology). The #74 attribution probes → the benches of the zones they
  isolate (model/parse for the parse pair, session/admit for
  encode/probe, store for the bare floor). `Workload::collect`'s
  iterator-vs-batched byte-equality claim → the trials differential. The
  program-builder plumbing follows its consumers into the bench rigs.
- **L2:** gold: the opaque-façade discipline ("no crate-internal type
  crosses the boundary" — the sealed-contract law implemented as a door)
  and hand-built `MagicProgram` construction that skips parse so the
  timed region is evaluation only. Condemned: `Backend::Mem` selects
  `SimStorage`, DST machinery bound for kyzo-trials — zone benches
  measure the real substrate; `Segments::OFF` and the
  panic-on-fixed-rules closure are door plumbing that dies with it.

## fuzz_api.rs (92 lines; inventory: module doc (opaque façade for the
fuzz targets, feature-gated, never hands out the AST; the memcmp-codec
target needs no façade because that surface is already public),
`fuzz_parse_script` (never-panic-never-hang law; Ok/Err both
acceptable), `fuzz_decode_fact_payload`, `fuzz_decode_relation_handle_id`
(discards the internal type, hands back the raw id),
`MAX_RELATION_ID` re-export, `interval_bounds` (the pub(crate)-accessor
seam) — closed)
- **L1:** the DOOR is deleted; the consumers rewire. `fuzz_parse_script`
  dies naturally: the parse tier becomes kyzo-model's PUBLIC boundary
  lift, so the fuzz target speaks it directly. The payload/catalog
  decode targets and the never-panic law → `crates/kyzo-trials/fuzz.rs` ("the
  ledger's corpus"), driving public store-contract seams.
  `interval_bounds` dies when Interval's accessors become model-public
  vocabulary.
- **L2:** gold: the discard-the-internal-type posture and the
  fuzz law stated as invariant, not coverage. DEFECT (doc-grade, check
  the target): the comment names the interval bypass invariant as
  `start < end`, but interval.rs's closed normal form makes singleton
  intervals (`start == end`) LAWFUL — if the fuzz target enforces
  strict `<` it false-positives on lawful values; if it checks `<=`,
  this comment misstates the law. One of the two must be corrected
  when the target rewires.

## lsp_api.rs (49 lines; inventory: module doc (validate-without-
executing for kyzo-lsp, story #92: parse + full resolution, `Err`
carries the exact designed diagnostic #73 built; same posture as the
other doors but NOT feature-gated — live diagnostics are a first-class
product surface), `check_script` (params map optional; an empty map
still validates everything and reports each `$name` unbound — "a real,
useful diagnostic on its own") — closed)
- **L1:** the DOOR is deleted; kyzo-lsp rewires onto kyzo-model's
  public parse surface (grammar + boundary lift + designed
  diagnostics live there in the target). Arrival question for the
  operator: full resolution includes FIXED-RULE references, and the
  registry (`DEFAULT_FIXED_RULES`) is engine-side — either the model's
  program vocabulary carries what a fixed rule IS (name/arity) so the
  LSP can validate references without the engine, or the LSP's
  validation story stays engine-coupled; the map should say which.
- **L2:** gold: diagnostics-as-product framing (the door's REASON
  survives even as the door dies); the empty-params ruling. Nothing
  condemned beyond the door itself.
