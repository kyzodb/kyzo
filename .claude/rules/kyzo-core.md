---
paths:
  - "kyzo-core/src/**"
---
# Rule: kyzo-core orientation

`kyzo-core` (package name `kyzo`) is the database engine. Map:

- `data/` — value model, **memcmp.rs** (on-disk key encoding), tuple layout
- `parse/` — pest grammar → `InputProgram` (the language is KyzoScript)
- `query/` — the Datalog compiler (see the query rule)
- `storage/` — the `Storage`/`StoreTx` trait + the single pure-Rust KV backend (see the storage rule)
- `runtime/` — `db.rs` entrypoint; hnsw/minhash_lsh/fts operators; transactions
- `fts/`, `fixed_rule/` — full-text search; built-in graph algorithms

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
