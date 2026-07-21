# Changelog

All notable changes to KyzoDB are documented here. The format follows [Keep a
Changelog](https://keepachangelog.com/en/1.1.0/); versioning follows [VERSIONING.md](VERSIONING.md)
(SemVer 2.0 on KyzoDB's own `0.Y.Z` line).

## [Unreleased]

Nothing yet.

## [0.9.0] — 2026-07-21

This wave makes the stored record accountable and seals the engine's type surface. It centers on
**KyzoRecord** — the admission unit — and **CanonicalTranscript**, the single serializer whose bytes
*are* a record's identity, feeding a MAC-signed, Merkle-rooted audit chain. Around it, a
codebase-wide campaign replaced ad hoc constructors, boolean/sentinel state, and panic gates with
sealed type-states, so illegal states no longer compile. Breaking: the on-disk sealed format and
several public façades changed. Still pre-1.0 — see the warning below.

### Added

- **KyzoRecord, the accountable unit**: admission produces a `KyzoRecord` (`session/admit.rs`) whose
  canonical bytes come from one authority, `CanonicalTranscript` (`store/transcript.rs`) — the sole
  sealed serializer on the stored surface. A record's identity is `SHA-256` over those bytes.
- **An audit chain over the transcript**: leaf MACs over each sealed `CanonicalTranscript`
  (`store/crypto.rs`), Merkle roots (`store/merkle.rs`), a forge wall, and grant/replica surfaces —
  every stored fact traceable to a signed, ordered leaf.
- **Sealed type-states across the engine**: checked `Validity` construction, `UuidWrapper` behind
  accessors, `NonZero`-backed `Arity` (compile-fail on zero), `Open`/`Committed`/`Aborted` `WriteTx`
  with typed `CommitFailure`, `EncodedKey` split into `TupleKey` and `StorageKey`, a `DomainCtx`
  required for raw-handle compare, and a `ProjectionBuilder → Sealed` generation machine.

### Changed

- **Storage backend is now publishable**: the vendored `fjall`/`lsm-tree` fork is packaged under the
  first-party names `kyzo-fjall`/`kyzo-lsm-tree` and `[patch.crates-io]` is dropped, so a published
  `kyzo` binds our fork (the `[lib] name` is unchanged — `use fjall::` still works). This replaces
  the patch routing shipped in 0.8.1.
- **Semantics moved into types**: expression evaluation rebinds to `Expr::eval`;
  `Semiring`/aggregation seal as enums with a private-supertrait `RuleBody`; the `MeetAggr` `Null`
  sentinel becomes `MeetAccum::{Empty, Value}`; `ensure_compatible` becomes a whole-schema
  `CompatibleInputSchema` proof; cross-`Domain` panics become typed refusals.
- **Repository layout and release**: the six first-party crates moved under `crates/`; the release
  workflow builds native binaries for linux/macOS/windows and publishes the crate chain in
  dependency order.

### Removed

- **The bytecode evaluator** and its call sites — evaluation goes through `Expr::eval`.
- **Duplicate and unsound surfaces** (the demolition campaign): the `cmp_numeric` second-order
  compare, mut-self transitions, the erased commit `Result`, `Drop`-as-abort, `admit_to` panic
  gates, `RelationHandle.is_temp`, `Watermark` `Option`-staleness, the `FtsIndexConfig` twin, raw
  trigger-source re-parse loops, and other second-authority residue — each cut whole, not silenced.

### Fixed

- Hardening surfaced by fuzzing: a hostile `Vector` length is refused before `with_capacity`
  (unbounded-allocation OOM), nested-set decode no longer runs `O(2^depth)` (canonicality checked in
  one pass), and the parser's list rule no longer backtracks exponentially on unterminated nested
  lists. Per the defect-ledger convention, individual fixes are not itemized pre-1.0.

## [0.8.1] — 2026-07-09

This wave lands two engine-level projects developed after the 0.8.0 tag: vendoring and tuning the
`fjall`/`lsm-tree` storage backend, and a ground-up rewrite of the in-memory execution value
representation (the "value plane"). It closes with the start of a full-codebase design census
against the target architecture.

### Added

- **Vendored storage backend**: `fjall` + `lsm-tree` 3.1.5 brought in as workspace members (routed
  via `[patch.crates-io]`), with per-keyspace LSM tuning (Monkey bloom-filter sizing, a measured
  Dostoevsky lazy-leveling step) and a RAM-proportional default cache size.
- **The value plane**: a new canonical cell format, an arena of epoch-sealed interned codes, and a
  typed execution-currency layer (admitted rows, code columns) replacing ad hoc per-row cloning on
  the hot evaluation path.
- **A type-authority graph and ratchet**: extracts closed-domain typed dispatch across the engine
  and audits raw-construction call sites and string/blob taxonomies, gating the build on the
  committed (narrowing-only) ratchet.
- **A pinned, containerized build/lint/test environment** (Docker + Compose), replacing
  host-native `ulimit`-based test execution.

### Changed

- Storage's SSI `seek_range` now tracks precise sub-ranges instead of reopening a full range scan
  per version step.
- `RelationId` and related storage-key vocabulary made unforgeable across the value plane.

### Fixed

- Defects surfaced by the value-plane rewrite and its accompanying property tests; not itemized
  individually pre-1.0, per the defect-ledger convention below.

## [0.8.0] — 2026-07-05

The first release: the storage kernel, the query engine, and the product built on it, each proven
against its own oracle before the next layer stood on it.

### Added

- **Storage kernel** (fjall backend): a `Storage`/`StoreTx` trait with one implementation —
  [`fjall`](https://github.com/fjall-rs/fjall), a pure-Rust LSM store — providing ordered range
  scans, MVCC commit with write-write conflict detection, and validity-in-key as-of reads (time
  travel) at the storage layer.
- **memcmp key encoding** (`crates/kyzo-core/src/data/memcmp.rs`): a memcomparable row/tuple encoding where
  bytewise key order equals semantic value order, the invariant every access path above storage
  relies on. FormatVersion 4 on disk.
- **Pure-Rust backup and interchange**: dump/restore of relations with no C/C++ dependency anywhere
  in the codec.
- **KyzoScript**: a Datalog dialect compiling through parse, normalize, stratify, magic-sets
  rewriting, relational algebra, and semi-naive fixpoint evaluation, over the storage kernel.
- **Query semantics proven against an independent oracle** (`crates/kyzo-core/src/query/laws.rs`): a
  deliberately naive reference implementation of stratified Datalog, compiled only into test
  builds; every generated workload is answered by both the optimized engine and the oracle and the
  two answers must be byte-identical.
- **The determinism law**: the same facts, query, and execution budget produce byte-identical
  answers and byte-identical refusals at any thread count (1, 2, 4, 8 threads exercised by the
  determinism campaign in the test suite).
- **Budgeted execution**: evaluation runs under explicit derivation-ceiling and deadline budgets;
  exceeding one yields a typed, deterministic refusal rather than a runaway query or a silent kill.
- **Provenance**: derived facts carry the rule and premises that entailed them, recursively down to
  ground facts, checked by an independent verifier that imports nothing from the evaluator.
- **Whole-graph algorithms as built-in rules**: PageRank, community detection (Louvain, label
  propagation), shortest paths (BFS, Dijkstra, A*, Yen's k-shortest, all-pairs), minimum spanning
  trees (Kruskal, Prim), strongly-connected components, k-core, maximal cliques, max-flow,
  centralities, and random walks — evaluated over ordinary relations, no export to a separate graph
  runtime.
- **Vector, text, and near-duplicate search as relational operators**: HNSW for approximate nearest
  neighbor search, full-text search with pluggable tokenizers and stemmers, and MinHash-LSH for
  near-duplicate detection, all queryable and joinable like any other relation.
- **Bitemporal time travel end to end**: every relation carries valid time and system time in the
  storage key; `@ instant` and `@ system, instant` as-of reads are ordinary seek-based range scans,
  proven at both the storage level and the query level against a unified temporal oracle.
- **`kyzo-bin`**: the standalone product — CLI, REPL, and an HTTP server (axum, tokio) — built with
  the same construct-by-construct standard as the kernel.
- **Whole-workspace CI**, pure Rust end to end (never core-only): purity gate (no C/C++ toolchain
  crate reachable from `kyzo-core`/`kyzo-bin`), `cargo-deny`, MPL-2.0 header preservation, `cargo
  fmt`/`cargo clippy -D warnings`, `cargo build`/`cargo test`, secret scanning, an unsafe-code
  ratchet (`#![forbid(unsafe_code)]` in every engine crate root), supply-chain vetting (`cargo
  vet`), a coverage ratchet, a memcmp on-disk-format tripwire, and fuzz smoke tests over the memcmp
  codec and the KyzoScript parser.
- **A defect ledger**: dozens of defects inherited from the fork base — including silent-wrong-answer
  bugs in recursive evaluation — found by these instruments, fixed, and pinned as permanent
  regression tests.

### Changed

- Replaced upstream CozoDB's RocksDB (C++) and SQLite (C) storage backends with the single
  pure-Rust fjall backend; the workspace builds with no C or C++ compiler anywhere in the
  `kyzo-core`/`kyzo-bin` dependency tree.
- Rebuilt the storage contract on the new backend: ordered scans, SSI over reads and writes,
  consuming commits, validity-in-key time travel — sealed and covered by contract tests rather than
  inherited unverified.

### Fixed

See the defect ledger above; individual fixes are not itemized per-release pre-1.0 — they are
covered by the regression suite that pins them.

[Unreleased]: https://github.com/kyzodb/kyzo/compare/v0.8.1...HEAD
[0.8.1]: https://github.com/kyzodb/kyzo/compare/v0.8.0...v0.8.1
[0.8.0]: https://github.com/kyzodb/kyzo/releases/tag/v0.8.0
