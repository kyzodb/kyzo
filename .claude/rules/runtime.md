---
paths:
  - "kyzo-core/src/runtime/hnsw.rs"
  - "kyzo-core/src/runtime/minhash_lsh.rs"
  - "kyzo-core/src/runtime/transact.rs"
  - "kyzo-core/src/fts/**"
---
# Rule: unified retrieval operators (HNSW / LSH / FTS)

Vector (HNSW), near-duplicate (MinHash-LSH), and full-text search are **first-class relational-algebra
operators** (`HnswSearch` / `LshSearch` / `FtsSearch` in `ra.rs`), not side APIs. Each takes a parent
tuple stream and extends every tuple with match columns plus a score/distance.

The property to preserve: an index search yields tuples that **join, filter, negate, and recurse like any
relation**. Do not let these become out-of-band calls that bypass the evaluator; that breaks the "one
substrate, many access paths" design that is the whole point of the engine.
