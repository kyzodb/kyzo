# Changelog

All notable changes to KyzoDB are documented here. The format follows [Keep a
Changelog](https://keepachangelog.com/en/1.1.0/); versioning follows [VERSIONING.md](VERSIONING.md)
(SemVer 2.0 on KyzoDB's own `0.Y.Z` line).

## [Unreleased]

Nothing yet.

## [0.1.0] — Unreleased

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

[Unreleased]: https://github.com/kyzodb/kyzo/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/kyzodb/kyzo/releases/tag/v0.1.0
