# KyzoDB and CozoDB

KyzoDB is a fork of [CozoDB](https://github.com/cozodb/cozo), created by
[Ziyang Hu](https://github.com/zh217) and the Cozo Project Authors. This document is the record of that
lineage: what KyzoDB inherited, what it changes, and how attribution is maintained. It exists so the
credit is stated once, completely, and in a place built to hold it.

## The debt

CozoDB is a rare piece of engineering: one declarative Datalog language over relational, graph, and
vector data — with full-text search, MinHash-LSH near-duplicate detection, and as-of time travel — all
composing because they rest on a single memcomparable, transactional key-value substrate. The insight
that one ordered key-value store can serve all of those access paths uniformly, the Datalog dialect that
exposes them as ordinary relations, the memcomparable row encoding, the stratified/semi-naive/magic-set
evaluation pipeline, the HNSW-index-as-relation design, and the storage abstraction that made this fork
tractable at all are CozoDB's work, not ours.

KyzoDB exists because that design deserved to keep going. Upstream development went quiet after 2023
(the last CozoDB release, v0.7.6, is from December 2023), and rather than let the design sit still, we
forked it and carried it forward. Fondly, and with thanks.

## What KyzoDB inherited

- The architecture: a query engine over a `Storage`/`StoreTx` trait, with language wrappers around a
  Rust core.
- The query language design (KyzoScript is a continuation of CozoScript) and its Datalog semantics.
- The memcomparable key encoding — the load-bearing invariant of the whole system.
- The design of HNSW, FTS, and MinHash-LSH as first-class relational operators, including the vendored
  pure-Rust tokenizers.
- Per-relation as-of time travel with validity in the key.
- The MPL-2.0 license.

## What KyzoDB changes

The full plan of record is [REFACTOR.md](REFACTOR.md); the work is tracked on the
[board](https://github.com/kyzodb/kyzo/issues). In brief:

- **Pure-Rust storage.** The RocksDB (C++) and SQLite (C) backends are replaced by
  [`fjall`](https://github.com/fjall-rs/fjall), a pure-Rust LSM store, behind the same storage trait.
  The engine and server build with no C or C++ toolchain.
- **Pure-Rust backup.** The SQLite-based backup/interchange format is replaced by a pure-Rust
  dump/restore format.
- **Rebuilt kernel-outward, nothing blind-copied.** Every inherited file is interrogated and lands only
  in its corrected final form, under adversarial review, mutation testing, differential oracles, and
  deterministic-simulation fault injection. Defects found in the inherited code are fixed and
  regression-pinned rather than carried.
- **New semantics.** An execution-budget model with typed, deterministic resource refusal; a determinism
  law (same facts, query, and budget produce identical answers and refusals at any thread count); typed
  span-carrying errors in place of reachable panics; and first-witness provenance, so a derived fact can
  name the rule and premises that entailed it.
- **The rebrand.** `cozo` becomes `kyzo` across the workspace and every language binding, so nobody
  mistakes the fork for the original or holds the original authors responsible for it.

## Attribution mechanics

- Every CozoDB copyright header and license notice is preserved verbatim in the files that carry
  inherited code; KyzoDB's notices are added alongside, never in place of them.
- Fixes incorporated from CozoDB contributors keep their original git authorship.
- KyzoDB remains MPL-2.0, the license CozoDB chose.

## What this fork is not

KyzoDB is an independent project. The CozoDB authors do not maintain, endorse, or answer for it. Bugs in
KyzoDB are ours; the foundation it stands on is theirs. If KyzoDB is useful to you, the people who made
it possible first are Ziyang Hu and the Cozo Project Authors — their project lives at
[github.com/cozodb/cozo](https://github.com/cozodb/cozo) and [cozodb.org](https://www.cozodb.org).
