---
paths:
  - "kyzo-core/src/**"
---
# Rule: kyzo-core orientation

`kyzo-core` (package name `kyzo`) is the database engine. Map:

- `data/` — value model; the three wire formats each in their own file: **memcmp.rs** (key
  encoding), **bitemporal.rs** (two-axis resolution kernel + claim polarity), **fact_payload.rs**
  (self-describing value layout); tuple layout; **batch.rs** (the columnar execution currency)
- `parse/` — pest grammar → `InputProgram` (the language is KyzoScript)
- `query/` — the Datalog compiler and evaluator, including the fixpoint state (`temp_store.rs`)
  and the columnar expression evaluator (`vm.rs`) (see the query rule)
- `storage/` — the `Storage`/`ReadTx`/`WriteTx` species + the single pure-Rust KV backend (see the storage rule)
- `engines/` — the derived-index engines: hnsw, fts, lsh, spatial, sparse, gazetteer
- `runtime/` — the session layer: `db.rs` entrypoint, the mutation tier (`mutate.rs`), the
  catalog (`relation.rs`), constraints, callbacks
- `fixed_rule/` — built-in graph algorithms (text analysis lives in `engines/text/`)

**Standing laws:**
- The engine is **pure Rust**: no C or C++ compiler in the `kyzo-core` / `kyzo-bin` build. Do not
  reintroduce a C/C++ dependency; that regresses the whole point of KyzoDB.
- The core is **isolated**: everything depends on `kyzo-core`; it depends on none of our crates. So
  `kyzo-core` + `kyzo-bin` can reach green before any binding. That ordering is forced by the dependency
  graph, not a licence to skip bindings.
- Prefer reproduce → change → verify. Do not mix unrelated cleanup into correctness work.
- **Type-driven over procedural**: invariants live in types, not runtime checks — smart constructors
  ("parse, don't validate"), immutable values transformed by consumption, and typestate for pipelines
  (a stage's output type is proof its checks passed; illegal states are unrepresentable). Push every
  law as far up the enforcement ladder as it can go: compiler > constructor > test.
- **The type graph is the world model** (crate docs in `src/lib.rs` are its artifact): every new or
  reshaped type is minted against the whole ontology — one name per concept, one concept per name —
  never against the convenience of a single file.
