# KyzoDB Refactor Plan

This is the plan of record for turning the CozoDB fork into KyzoDB. It is the "why" behind every story
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
validity-in-key as-of scans (time travel); these are proven on `fjall` first, in the storage-kernel story, before anything depends on them. The
memcomparable key encoding (`data/memcmp.rs`) stays; it is the load-bearing invariant and the reason a
dumb ordered KV can serve relational, graph, vector, and text access paths uniformly.

The bet on fjall's *storage* layer (the LSM) is unconditional; its *transaction* layer is the current
choice with a measured contingency — owning the oracle ourselves if benchmarks show its commit ceiling
binding (decision record and trigger live on the board, #4).

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

KyzoDB's **backup/interchange format** is a **pure-Rust dump/restore**: a length-prefixed portable
file carrying the store's on-disk format version, restored atomically in chunks into an empty store.
The cozo base used SQLite for this role.

## 6. The stories (tracked on the board)

The build order is **kernel-outward**: the codebase grows from a proven storage kernel, and every file
lands in its exact end-state form. Nothing arrives that is not in the target; there is never an old
backend, a compatibility layer, or an interim state to remove later. Copying from upstream is allowed,
**blind copying is not**: every construct is interrogated — *is this the best way, does it even
belong?* — and lands only as its best version, with the hard work done first, not deferred. The
guardrails (`.claude/`) and the CI gates exist before any code, so every story lands inside the
machine-checked envelope.

- **Storage kernel (#2).** The smallest compiling unit that proves the fork's one real bet. The memcmp
  encoding and the minimal value/tuple types it encodes, the `Storage`/`StoreTx` trait in final form,
  the `fjall` backend (ordered scans, MVCC commit with conflict detection, validity-in-key as-of reads),
  the pure-Rust backup, and contract tests for every property. Compiles and passes green on day one; the
  CI gates activate on real code here.
- **Engine (#3).** Parser, Datalog compiler pipeline, runtime (HNSW / MinHash-LSH / FTS as first-class
  relational-algebra operators), and graph algorithms grow around the proven kernel — each file in final
  `kyzo` form, MPL headers preserved.
- **Product green (#4).** `kyzo-bin`, the full inherited test suite passing, and time travel verified
  end-to-end at the query level. Coverage, fuzz, and tripwire gates activate. Every binding depends on
  this gate.
- **Bindings, in-workspace (#5-#10):** C, Python (PyPI), Java (Maven), Node (npm), Swift, WASM (npm).
  Each: rework FFI, rebrand, build, test, publish.
- **Bindings, separate repos (#11-#14):** Go (wraps the C ABI, needs #5), Clojure (JVM, needs #7),
  Android (#13), and the Python client (needs #6).

Dependency order is forced: #2 -> #3 -> #4 in sequence; bindings after #4; Go after #5, Clojure after
#7, the Python client after #6.

## 7. Principles (earned the hard way)

- Verify every claim against a real build/test/run or the actual file. No conclusions from memory.
- Never narrow scope to produce a clean number; whole-workspace, or say it is partial.
- Bindings are committed, not deferrable. Name hard work; do not smuggle avoidance into recommendations.
- One coherent target per story; no interim split-brain.
- Nothing public or irreversible without an explicit go.

## 8. Backlog

Post-green re-triage of the fork base's surviving engine issues is tracked on the board (#18); the
storage/backend issues this refactor moots were dropped.
