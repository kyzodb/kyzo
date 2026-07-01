---
paths:
  - "kyzo-core/src/storage/**"
---
# Rule: the single pure-Rust KV backend & the Storage/StoreTx contract

KyzoDB has **one** storage backend: **`fjall`, a pure-Rust LSM key-value store**, implementing the
`Storage`/`StoreTx` trait (`storage/mod.rs`). `fjall` is the decided choice (a pure-Rust, RocksDB-shaped LSM that keeps
write concurrency); do not swap in `redb` (single-writer) or `sled` (unstable) without re-opening that
decision. This rule is about the contract the backend must honour.

The contract:
- **Ordered range scans** returning memcmp-ordered tuples (the memcmp encoding is why this works).
- **MVCC-style commit** with write-write conflict detection (`commit()` fails on conflict; `for_update`).
- **Time travel = a validity stamp in the last key slot**: an as-of scan returns the newest version at or
  before the query time. Preserve it; it is not optional.
- **Pure Rust**: no C or C++ toolchain. Do not reintroduce one.

The backup/interchange format is a pure-Rust dump/restore (the cozo base used SQLite for this role).

**A change here requires:** checking it against the contract (scans, MVCC, validity-in-key time travel),
a test that exercises as-of reads, and confirming no C/C++ dependency sneaks back in.
