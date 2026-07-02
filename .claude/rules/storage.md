---
paths:
  - "kyzo-core/src/storage/**"
---
# Rule: the single pure-Rust KV backend & the transaction species

KyzoDB has **one** storage backend: **`fjall`, a pure-Rust LSM key-value store**, implementing the
`Storage` / `ReadTx` / `WriteTx` traits (`storage/mod.rs`). `fjall` is the decided choice (a pure-Rust,
RocksDB-shaped LSM that keeps write concurrency); do not swap in `redb` (single-writer) or `sled`
(unstable) without re-opening that decision. This rule is about the contract the backend must honour.

The contract:
- **Ordered range scans** returning memcmp-ordered tuples (the memcmp encoding is why this works).
- **Two transaction species, enforced by types, not checks**: a `ReadTx` is one consistent snapshot and
  cannot write *by construction*; a `WriteTx` extends reading with a conflict-tracked write set, and
  `commit`/`commit_durable` **consume** it. Write-on-reader and commit-twice are not error paths — they
  do not compile. Never reintroduce a runtime guard where the type system already forbids the state.
- **MVCC commit with conflict detection (SSI)**: every read and range in a write transaction is
  conflict-tracked; commit fails with the typed, retryable `ConflictError` — discarding all changes —
  on conflict. Liveness is `retry_on_conflict`.
- **Time travel = a validity stamp in the last key slot**: an as-of scan returns the newest version at
  or before the query time, by seeking, with guaranteed termination on any stored bytes. Not optional.
- **Pure Rust**: no C or C++ toolchain. Zero `unsafe` (compiler-enforced via `#![forbid]`).

Durability is explicit and typed: `commit` survives a process crash, `commit_durable`/`sync` survive a
power cut. Stores and dumps are stamped with `FormatVersion`; mismatches refuse to open. The
backup/interchange format is a pure-Rust dump/restore (the cozo base used SQLite for this role).

**A change here requires:** checking it against the contract (scans, species semantics, SSI,
validity-in-key time travel), a test that exercises as-of reads, confirmation that no invariant moved
DOWN the enforcement ladder (compiler > constructor > test), and that no C/C++ dependency sneaks in.
