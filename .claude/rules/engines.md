---
paths:
  - "kyzo-core/src/engines/**"
  - "kyzo-core/src/query/ra/**"
---
# Rule: unified retrieval operators (the engines tier)

Vector (HNSW), near-duplicate (MinHash-LSH), full-text, spatial, sparse, and gazetteer search are
**first-class relational-algebra operators** (the unified `SearchRA` in `query/ra/search.rs`), not side
APIs. Each takes a parent tuple stream and extends every tuple with match columns plus a
score/distance. Columnar current-state segments (`engines/segments.rs`) are the same species: a
rebuildable acceleration structure, never a second source of truth — validity is watermark identity.

The property to preserve: an index search yields tuples that **join, filter, negate, and recurse like any
relation**. Do not let these become out-of-band calls that bypass the evaluator; that breaks the "one
substrate, many access paths" design that is the whole point of the engine.
