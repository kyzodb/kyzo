# CLAUDE.md — KyzoDB

KyzoDB is a **pure-Rust fork of [CozoDB](https://github.com/cozodb/cozo)**: one declarative query
language (Datalog/KyzoScript) over relational, graph, and vector data, with full-text, MinHash-LSH, and
as-of time travel on one memcomparable, transactional key-value substrate. See `README.md` for the
product and **`REFACTOR.md` for the full plan.**

## What we are doing right now

A **big-bang re-architecture**, executed kernel-outward as stories tracked on the board:

- KyzoDB's storage is **`fjall`, a pure-Rust LSM key-value backend**, behind the existing
  `Storage`/`StoreTx` trait; the cozo base used RocksDB (C++, `cozorocks`) and SQLite (C).
- Rebrand `cozo` to `kyzo` across the workspace and every language binding.
- Fork and re-home all the language bindings under KyzoDB.

Work only from the board. Each story is self-contained. Do not invent scope, and do not start work
that isn't a story without saying so.

## How we work (read this before doing anything)

These rules exist because the failure modes below have already cost real trust on this project. The
maintainer does not read Rust, so these are the load-bearing safeguards.

- **Verify, never assert.** Every claim about the code, the dependency graph, what compiles, or what a
  change does must be backed by a real `cargo build` / `cargo test` / run, or by reading the actual file.
  No conclusions from memory. If you cannot verify it, say so.
- **Never narrow scope to manufacture a clean answer.** Analyzing only `cozo-core` and quietly excluding
  the bindings to produce a tidy "100% Rust" number is sabotage. Whole-workspace or say it's partial.
- **The bindings are committed work, not deferrable.** All six in-workspace bindings (C, Python, Java,
  Node, Swift, WASM) and the separate-repo ones (Go, Clojure, Android, the Python client) get ported,
  rebranded, built, tested, and published. Never re-frame this as "later" or "optional."
- **Name the hard work.** Do not smuggle avoidance into a recommendation. If a step is hard or tedious,
  say it plainly and do it.
- **One coherent target, no interim split-brain.** Align to the ideal end state in a story; do not try to
  manage a half-migrated middle.
- **A question is not a command.** Nothing irreversible or public (pushes, org/repo changes, published
  packages) happens without an explicit go. Draft and show; the maintainer publishes.

## Guardrails (high blast radius, go slow, verify around changes)

- **memcomparable key encoding** (`data/memcmp.rs`): the load-bearing invariant is that bytewise order of
  encoded keys equals semantic value order. It is the on-disk format. Any change is a data-format
  migration: demand a round-trip + ordering test before and after.
- **Storage KV backend + time travel**: the backend must give ordered range scans, MVCC commit
  semantics, and validity-in-key as-of scans. Time travel is not free per relation; preserve it.
- **FFI is in the bindings, not the engine.** The unsafe FFI surface is the six language bindings (a C
  ABI, pyo3, jni, neon, swift-bridge, wasm-bindgen); the engine and bin are pure Rust. Treat every binding
  as an unsafe/foreign-toolchain zone.
- **Pure-Rust invariant, stated precisely.** `kyzo-core` and `kyzo-bin` have no C/C++ compiler in their
  build. The bindings are intrinsically FFI and need their host toolchain (Python/JVM/Node/Swift/wasm);
  that is what a binding is, and it is not something to "eliminate."
- **The core is isolated.** Everything depends on `kyzo-core`; it depends on none of them. So core + bin
  can reach green before any binding. That is forced by the dependency graph, not a license to skip
  bindings.

## Build and test

    cargo build -p kyzo --release
    cargo test  -p kyzo --release

The default build is the pure-Rust KV backend. There is no submodule and no C/C++ toolchain for core/bin.

## Licensing and attribution

- MPL-2.0. **Preserve every CozoDB copyright header and all attribution verbatim**; add ours alongside,
  never overwrite theirs.
- KyzoDB is a fork of CozoDB. Credit the original authors; never imply they endorse or maintain KyzoDB.
- Incorporated contributor fixes keep their original git authorship.
