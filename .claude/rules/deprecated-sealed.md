---
paths:
  - "kyzo-core/src/bench_api.rs"
  - "kyzo-core/src/fuzz_api.rs"
  - "kyzo-core/src/lsp_api.rs"
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
  seeded workload constructors + `Graph` shapes → `kyzo-core/benches/`
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
