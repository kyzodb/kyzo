---
paths:
  - "kyzo-core/src/storage/**/*.rs"
  - "kyzo-core/src/runtime/relation.rs"
  - "kyzo-core/src/data/relation.rs"
  - "kyzo-core/src/data/value/wide/json.rs"
  - "kyzo-core/src/data/bitemporal.rs"
---

# Storage Contract & Serialization Authority

ONE backend: `fjall`, a pure-Rust LSM, implementing `Storage`/`ReadTx`/`WriteTx`. Swapping it out
(redb, sled) re-opens a decided question.

## The contract (invariants held by TYPES, not runtime checks)

- Ordered range scans return canonical-byte-ordered tuples.
- `ReadTx` cannot write by construction; `WriteTx` conflict-tracks reads and `commit` CONSUMES it.
  Write-on-reader and commit-twice do not compile. Never reintroduce a runtime guard where the type
  system already forbids the state.
- MVCC + SSI: every read/range in a write tx is conflict-tracked; commit fails with the typed
  retryable `ConflictError`, discarding all changes.
- Bitemporal time travel is mandatory: each fact key ends with two fixed-width slots (valid outer,
  system inner); claim polarity rides in the VALUE, never the key. As-of resolves at `AsOf { sys,
  valid }` by seeking, terminating on any bytes; a flag-bearing slot refuses as corruption.
- Stamp minting is snapshot-then-mint (the mint takes the open snapshot as an argument, so the
  reverse is unrepresentable). The clock floor is a monotone watermark.
- `commit` survives a process crash, `commit_durable`/`sync` a power cut. Stores/dumps are stamped
  with `FormatVersion`; mismatches refuse to open.

## Serialization authority

The value plane has ONE value serialization authority: **canonical bytes**. Any other serialization
boundary must be explicitly RULED. Allowed non-value boundary (e.g. the sealed msgpack catalog door,
index manifests riding inside it): config-only metadata, NO `DataValue`; one sealed door, named
`FormatVersion`, typed corruption behavior; tests proving no value-plane authority crosses it.

Forbidden: "metadata only" as an argument; multiple standalone `rmp_serde` persistence doors; a
`DataValue` through msgpack; a catalog format that becomes a second value authority; silent
compatibility deserialization.

## A change here requires

Checking against the contract; an as-of read test; no invariant moved DOWN the ladder (compiler >
constructor > test); no C/C++ dependency introduced.
