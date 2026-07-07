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
- **Time travel is bitemporal (mandatory, no single-axis past)**: every fact key ends with TWO
  fixed-width slots — valid instant (outer) and system version (inner), flags pinned to assert — and a
  row's claim polarity (Assert / Retract / Erase, `data/bitemporal.rs::ClaimPolarity`) rides in the
  VALUE, never the key, so one valid instant has exactly one system lineage. An as-of scan
  (`range_skip_scan_tuple`) resolves at an `AsOf { sys, valid }` coordinate by seeking, with
  guaranteed termination on any stored bytes; a flag-bearing slot refuses as corruption.
- **Stamp minting is snapshot-then-mint, and the order is load-bearing**: `write_tx` takes the fjall
  snapshot FIRST and mints the system stamp SECOND (the mint takes the open snapshot as an argument,
  so the reverse is unrepresentable). That ordering alone proves reads-from order agrees with stamp
  order; minting first shipped a silent lost-update once.
- **The clock floor is a monotone watermark** (`Storage::{clock_floor, raise_clock_floor}`): restore
  raises the target's floor to the dump's before importing so imported instants can never be
  re-minted; the persisted watermark writes under a lock so it never regresses under concurrent
  mints. Bulk import (`batch_put`) is OUTSIDE the stamp/SSI surface and both backends refuse a
  non-empty target.
- **Pure Rust**: no C or C++ toolchain. Zero `unsafe` in `kyzo-core`, compiler-enforced: the crate
  root is `#![forbid(unsafe_code)]` — the maximum standard, with ZERO exceptions and no
  locally-liftable `#[allow(unsafe_code)]`. The value plane, including `GermanStr`'s 16-byte layout,
  is pure safe Rust (`GermanStr` is a safe wrapper over the 16-byte value cell). A future story that
  genuinely needs unsafe must lower the lint deliberately in that story, at the narrowest scope, with
  a full safety case; until then, unsafe does not exist in this crate. `scripts/check-unsafe.sh`
  enforces the lint, the zero-`allow` rule, and that no doc claims an exception that does not exist.

Durability is explicit and typed: `commit` survives a process crash, `commit_durable`/`sync` survive a
power cut. Stores and dumps are stamped with `FormatVersion`; mismatches refuse to open. The
backup/interchange format is a pure-Rust dump/restore (the cozo base used SQLite for this role).

**A change here requires:** checking it against the contract (scans, species semantics, SSI,
validity-in-key time travel), a test that exercises as-of reads, confirmation that no invariant moved
DOWN the enforcement ladder (compiler > constructor > test), and that no C/C++ dependency sneaks in.
