# KyzoDB Refactor Plan

This is the plan of record for turning the CozoDB fork into KyzoDB. It is the "why" behind every slice
on the board.

## 1. Why

CozoDB is a rare piece of engineering: one declarative query language over relational, graph, and vector
data, with full-text, MinHash-LSH near-duplicate, and as-of time-travel retrieval, all on a single
memcomparable, transactional key-value substrate. That coherence is exactly what knowledge-heavy and
agent-facing retrieval needs. Upstream has been quiet since 2023.

The single heaviest liability was never the design; it was the **C/C++ ballast**. RocksDB (C++, via the
`cozorocks` bridge) and SQLite (C, via `sqlite3-src`) drag a C/C++ toolchain through the build and every
binding, and RocksDB alone is a vendored submodule that breaks on new compilers (we hit and fixed a GCC
16 build break). That toolchain tax is a plausible cause of the project stalling under one maintainer.

Both liabilities are removable because Ziyang Hu isolated storage behind a `Storage`/`StoreTx` trait. The
engine does not know or care what is underneath. So swapping the backend is a contained change, not a
rewrite.

## 2. The architectural move

KyzoDB is built on **`fjall`, a pure-Rust LSM key-value store**, implementing the existing
`Storage`/`StoreTx` trait; the cozo base used RocksDB and SQLite. `fjall` is the choice because RocksDB was there for write concurrency and `fjall`
is the pure-Rust LSM engine (RocksDB-shaped) that keeps it; `redb` is a single-writer copy-on-write
B-tree that would re-impose SQLite's one-writer wall we are escaping, and `sled` is pure Rust but stalled
and unstable. The trait needs ordered range scans, MVCC-style commit with conflict detection, and
validity-in-key as-of scans (time travel); these are proven on `fjall` in the Cutover slice. The
memcomparable key encoding (`data/memcmp.rs`) stays; it is the load-bearing invariant and the reason a
dumb ordered KV can serve relational, graph, vector, and text access paths uniformly.

## 3. What "pure Rust" precisely means (verified, whole-workspace)

- **`kyzo-core` (engine) and `kyzo-bin` (CLI/HTTP server) are genuinely pure Rust.** No C/C++ compiler in
  the build. The core is fully isolated: everything depends on it; it depends on none of our crates.
- **The language bindings remain intrinsic FFI** and each carries `unsafe` code plus a foreign toolchain:
  a C ABI (`cbindgen`), Python (`pyo3`), Java (`jni`), Node (`neon`/napi), Swift (`swift-bridge`), WASM
  (`wasm-bindgen`). You cannot make a Python binding "pure Rust"; `pyo3` is the binding. The bindings
  carry no C/C++ storage build, only their own FFI. This is committed work, not something to eliminate or
  defer.

## 4. Custom-crate findings (verified against the lockfile and the cozodb org)

- Hu published several Rust crates (`cozorocks`, forks of `cang-jie` and `tantivy-tokenizer`,
  `networkit-rs`), but only **`cozorocks` is an external dependency**, and it (the base's C++ bridge) is
  not carried over.
- The FTS tokenizers are **vendored in-tree** (`fts/cangjie`, `fts/tokenizer`), pure Rust; they stay.
- `networkit-rs` (a C++ graph wrapper) is **not used**; graph algorithms use the third-party
  `graph_builder`.
- So the engine carries zero external custom C/C++ crates.

## 5. The one non-obvious addition

KyzoDB's **backup/interchange format** is a **pure-Rust dump/restore** (serialize relations via the
existing `rmp`/serde encoding, or a simple portable file); the cozo base used SQLite for this role.

## 6. The slices (tracked on the board)

- **Slice 0 — `.claude/` control surface, first.** Author the guardrails aimed at the target state before
  any code moves, so they watch the risky work instead of arriving after it. The FFI guardrails cover the
  six language bindings; the skills are the ones this work needs.
- **Slice 1 — Scaffold.** Copy pure-keeper files (logic unchanged, incl. the Slice-0 `.claude/`), add new
  stub files, one uniform `cozo` to `kyzo` rename, preserve MPL headers. Will not compile yet.
- **Slice 2 — Surgical files.** The files that change (`storage/mod.rs`, `lib.rs`) land already stripped
  of the base's backends; wire **`fjall`** as the KV backend + the pure-Rust backup, update
  dispatch/variants/features. The base's RocksDB/SQLite/`cozorocks` files are do-not-bring and never arrive.
- **Slice 3 — Green.** Get `kyzo-core` + `kyzo-bin` to build and pass tests, pure Rust, time travel
  verified. This is the gate every binding depends on.
- **Slices 4-9 — bindings (in-workspace):** C, Python (PyPI), Java (Maven), Node (npm), Swift, WASM (npm).
  Each: rework FFI, rebrand, build, test, publish.
- **Slices 10-13 — bindings (separate repos, forked):** Go (wraps C, needs Slice 4), Clojure (JVM, needs
  Slice 6), Android, and the `pycozo` Python client (needs Slice 5).

Dependency order is forced: Slice 0, then 1, 2, 3 in sequence; bindings after Slice 3; Go after 4,
Clojure after 6, Python client after 5.

## 7. Principles (earned the hard way)

- Verify every claim against a real build/test/run or the actual file. No conclusions from memory.
- Never narrow scope to produce a clean number; whole-workspace, or say it is partial.
- Bindings are committed, not deferrable. Name hard work; do not smuggle avoidance into recommendations.
- One coherent target per slice; no interim split-brain.
- Nothing public or irreversible without an explicit go.

## 8. Backlog

The storage/backend issues this refactor moots (RocksDB config, SQLite version/perf, sled, redb) were
dropped. The surviving engine issues from the fork base (parser and data-value bugs, the UUID
memcomparable-sortability issue, dependency hygiene, exposed private AST types, vector/FTS behaviour) are
real post-green work and will be re-triaged as native KyzoDB issues once the migration is green. They are
not part of the migration slices.
