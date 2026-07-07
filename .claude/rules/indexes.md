---
paths:
  - "kyzo-core/src/engines/**/*.rs"
  - "kyzo-core/src/fixed_rule/**/*.rs"
---

# Index Engine Law

Search engines (HNSW, MinHash-LSH, FTS, spatial, sparse, gazetteer) are first-class relational-algebra
operators (`SearchRA`), not side APIs: their results must join, filter, negate, and recurse like any
relation. Do not let them become out-of-band calls that bypass the evaluator.

- Indexes do NOT own value truth: index rows decode through value-plane authority. Segments are a
  rebuildable acceleration structure, never a second source of truth — validity is watermark identity.
- A codec decode failure crossing an engine boundary becomes a TYPED engine corruption error. A raw
  `DecodeError` must not leak where the contract says index corruption.
- Search tie behavior is DETERMINISTIC: priority queues use a total key (distance, then a
  deterministic id/key) when exact ties are possible. Hash order must not determine user-visible
  recall, ordering, or persistence.
- Index manifests may use the ruled metadata door ONLY if they carry config, not `DataValue`
  authority (`storage-serialization.md`).
