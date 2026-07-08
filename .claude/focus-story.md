# KyzoDB Work Management

You track work on the board, and only there. Do not use your task manager;
tasks live in stories. Do not keep notes in your scratchpad; working notes are
tight, informative story comments. When a story's strategy evolves, rewrite
the body — do not append a contradicting comment trail.

The board must match reality every turn. A story you are working on is In
Progress; a completed story is moved to Done. Move the card the moment reality
changes — `scripts/move-story.sh <n> <todo|focus|done>` (done also closes the
issue). You do this to give yourself the safety operator oversight affords you.

## Queued on the board (Todo) — clear their runway

### #120 — Evaluator rebuild: admitted-row currency, packed interned tuples, WCOJ, deterministic parallel fixpoint

### #121 — Runtime fast path: parameter-sensitive plan cache, sorted write batches, delta constraints, streaming results

### #122 — Engine residency: rebuildable resident projection structures over canonical storage

### #123 — Vector projection compute: RaBitQ-first search, deterministic filtering, allocation-free hot path

### #124 — Posting lattice: text/sparse/LSH postings as typed projection acceleration

### #125 — Geometry projection: best-first Z-curve kNN and spatial pruning

### #10 — WASM runtime package: browser-safe engine envelope and npm surface

### #72 — Hosting: Durable Objects storage backend — replay is the persistence mechanism

### #8 — Binding kyzo Node (neon/napi -> npm)

### #6 — Binding kyzo Python (pyo3 -> PyPI wheel)

### #5 — Binding kyzo-lib-c (C ABI via cbindgen)

### #7 — Binding kyzo Java (jni -> Maven)

### #9 — Binding kyzo Swift (swift-bridge)

### #11 — Fork kyzo-lib-go (separate repo, wraps the C ABI)

### #12 — Fork kyzo-clj (separate repo, JVM)

### #13 — Fork kyzo-lib-android (separate repo)

### #14 — Fork pycozo -> kyzo Python client (separate repo)

### #39 — Epic: Benchmarks

### #40 — Epic: Trials (adversarial correctness proofs)

### #32 — Trial: the proof audit (verify every derivation's witness at scale)

### #33 — Trial: the fuzzing ledger (published CPU-months, zero panics)

### #41 — Epic: Demos

### #96 — ::verify: cover expression/filter queries via an independent oracle expression evaluator

### #97 — Continuous bug-hunters in CI: cargo-mutants + Miri + ThreadSanitizer

### #98 — Fuzzing: extend beyond parser/codec to end-to-end run_script + HTTP

### #99 — Cross-engine correctness differential (results, not just perf)

### #100 — Doctests on the public API

### #42 — Engine capability: text and entities (gazetteer tagging, fuzzy, phonetic)

### #43 — Engine capability: reactive (standing queries, incremental views, change feeds)

### #51 — Epic: Engine capabilities

### #44 — Engine capability: spatial (space-filling-curve geo index, geometry predicates)

### #45 — Engine capability: temporal (interval algebra, history diff, bitemporality)

### #46 — Engine capability: graph (missing algorithms, motif matching, path queries)

### #47 — Engine capability: vector (filtered HNSW, quantization, sparse vectors, rank fusion)

### #49 — Engine capability: integrity and trust (constraints, semiring provenance, Merkle root, graph diff/merge)

### #48 — Engine capability: analytics (window functions, deterministic sketches)

### #50 — Engine capability: extensibility and interop (WASM UDFs, Arrow/Parquet)

### #22 — Bench: recursive Datalog vs Souffle (TC, same-generation, points-to)

### #23 — Bench: LDBC Graphalytics vs Kuzu (whole-graph algorithms)

### #24 — Bench: LDBC SNB Interactive, single-node scope

### #25 — Bench: ann-benchmarks and the big-ann filtered track (vector)

### #26 — Bench: embedded OLTP vs SQLite (mixed read/write)

### #27 — Bench: full-text vs Tantivy standalone and FTS5, and FTS inside joins

### #28 — Bench: define the time-travel benchmark (as-of overhead vs history depth)

### #35 — Demo: consistency under concurrent writes (one transaction vs a stitched pipeline)

### #36 — Demo: the Raspberry Pi replay (byte-identical across hardware)

### #37 — Demo: full engine in WASM, hash-matching native

### #38 — Demo: answer, proof, retraction, and as-of in one loop

### #63 — Provenance, counted and weighted

### #64 — Verifiable graphs: state roots, diff, and merge

### #66 — Federation semantics, specified in the open

### #67 — kyzo-bench: stand up the public proving ground (execution story for the trials batch #22–#28, #35–#38)

### #71 — Bench: onboard SurrealDB as a comparative subject across kyzo-bench

### #90 — Operator tier: coherent multi-row catalog move (unblock ::rename on indexed relations)

### #102 — Provenance as a first-class explanation surface: verified, name-resolved, human/agent-dual proofs

### #126 — Tuple newtype hardening: remove bare Vec<DataValue> row authority

### #127 — Graph traversal: resident canonical CSR and direction-optimizing BFS

### #128 — KyzoRecord foundation spike: typed accountable knowledge unit over KyzoDB

### #129 — Official reproducible gate environment

### #130 — Kyzo client encryption foundation: zero-access encrypted sync, client key custody

### #131 — Local-first topology manager: per-scope databases, manifests, hosted promotion

### #132 — Open federation foundation: identity/namespace, record stream + replay contract, capability negotiation

### #133 — KyzoMem: accountable conversation memory over KyzoRecord

### #134 — KyzoKnow: managed accountable RAG over KyzoRecord

### #135 — Catalog validity generations: global CatalogGeneration + scoped RelationGeneration/IndexGeneration

### #88 — Engine 0.9.0 release: seal the whole to pristine, or refuse to ship

### #138 — Engine capability: denial-rule constraints (FK / CHECK / uniqueness parity)

### #141 — WASM/browser/edge runtime substrate: one envelope for playground, demo, and Workers

### #142 — Bindings and client distribution: sealed API consumers

## Focus — execute this contract completely

### #82 — Post-#119 OLTP mixed-op benchmark census and root-cause routing
Label:  | Milestone: Version 0.9.0 | Epic: none

Re-establish ground truth for the OLTP mixed-op path on post-#119 main, classify the root cause, and route the work. A measurement + routing story, not a fix.

## 1. Current code evidence
- The figures in this issue's history — "~2.5-3x SQLite (per #74) → ~920x / non-terminating at 7447589" — were measured on pin `7447589`, a PRE-value-plane tree (before #118/#119).
- Filed from the kyzo-bench #26 rig; landed record `results/oltp--oltp_r100k-o20k--kyzo_744758991096--seed26101--2026-07-04.json` (the r1m-o100k workload never completed, so no file).
- No post-#119 measurement of this path exists.

## 2. Architectural smell
A headline regression number is being carried as current truth across a rewrite (#118/#119) that changed per-operation cost. It is stale until re-measured; citing it as current is a false claim.

## 3. Required invariant
- No pre-#119 number is cited as current.
- A comparison to a baseline is made only after confirming the benchmark still measures the same operation (same op mix, same public door, same SQLite-side shape).
- This story routes work; it does not fix performance unless the defect is purely benchmark/harness.

## 4. Acceptance criteria
- The OLTP mixed-op benchmarks are rerun from a clean kyzo-bench checkout pinned to post-#119 main (exact commit recorded).
- The full reproducibility envelope is recorded: commit, CPU, memory ceiling, target dir, feature flags, test threading, container image, command line, seed.
- A single root cause is classified with evidence.
- Implementation work is routed to the owning story (linked); no fix lands here unless harness-only.

## 5. Non-goals
- No performance fix in #82 unless the finding is a benchmark/harness correction.
- No citing the pre-#119 numbers as current.
- No comparison against a baseline that no longer measures the same operation.

## 6. Dependencies
- Upstream: none.
- Feeds: routes to #126 (Tuple fallout), #120 (evaluator/temp-store), #121 (runtime plan/write path), or storage. The #68/#120 kill-threshold re-baseline depends on this census.

## 7. Benchmarks / proofs
- The landed post-#119 measurement with the full envelope.
- The classification with evidence.
- If harness defect: fixed here, re-measured, closed.
- If no longer reproduces: closed with the post-#119 evidence showing it.

## 8. Open decisions
- The root-cause bucket: value-plane overhead / Tuple-#126 fallout / evaluator-#120 temp-store / runtime-#121 plan/write / storage-write / benchmark-harness defect / stale-no-longer-reproduces.
- Whether #68's original kill thresholds (12 GiB cap, ≤4× Souffle RSS) are adopted post-#119 or re-set — this census sets the bar #120 inherits.

## Hardest obligation
Failure mode: "optimize a little while we're here" against a stale baseline, or reporting a re-measurement as if it were a fix — chasing a number that no longer measures the same operation, or hiding a real regression behind re-measurement framing.
Invariant: measure first on post-#119 main, route to the owning story, no fix here unless harness-only; no pre-#119 number cited as current.
Proof: the landed post-#119 envelope + the routed, evidence-backed classification.


#### Comments
**kylejtobin (2026-07-04):**
**Root cause confirmed, fix landed in-tree (story-62 branch, seals with the story), numbers below.**

The mixed-op collapse was current-state segment-cache thrash: every write bumps the relation watermark (correctly — that is the serve-only-on-identity soundness rule), so under interleaved read/write every read missed the cache and triggered a FULL relation rescan + segment rebuild — O(n) per op, quadratic over the run. The load phase never sees it (no reads); pure-read benches never see it (no watermark churn); exactly the mixed phase pays.

**Fix**: a rebuild gate (`SegmentEngine::should_build`, engines/segments.rs) — a segment is built only after 2 consecutive misses at the SAME witness, i.e. two reads with no intervening committed write. Write-interleaved readers never cross the gate and fall back to the plain scan path (correct, just per-probe slower); read-stable phases build on the second miss and serve thereafter. The gate decides only WHEN TO BUILD, never what to serve — the serve check remains exact watermark identity, so no path can serve stale; the miss map is explicitly disposable state (losing it delays a rebuild, never corrupts an answer). Constitution intact: rebuildable, never a second source of truth, serve-only-on-identity.

**Numbers** (examples/oltp_mixed_profile.rs, 500 mixed ops):
| relation size | always-rebuild (the regression) | gated (fix) |
|---|---|---|
| r2,000 | 755.7 ops/s | 29,286.4 ops/s |
| r5,000 | 284.3 ops/s | 30,259.4 ops/s |
| r10,000 | 136.8 ops/s | 29,573.1 ops/s |

The flatness across relation size is the structural proof (amortized O(1)/op); the old curve degrades with n. Order-of-magnitude, this returns the mixed path to the pre-regression "~2.5-3x SQLite" class the issue cites (SQLite ~48.8k ops/s at r100k — different scale, directional comparison only).

Proven by: 5 new tests through the real StoredRA production path (never-build-under-interleaved-writes; build-at-threshold-and-serve; reset-on-intervening-write; a 40-round seeded differential of gated-out vs served vs an independent model, byte-identical throughout; miss-map-loss harmlessness) + 2 mutants (always-build; broken witness-reset), both killed by multiple named tests. Full capped suite 939/0/7, clippy both feature configs, fmt clean.

Issue stays open until the story-62 seal lands on main and kyzo-bench re-runs the r100k-o20k / r1m-o100k matrix against it — their re-measurement is the closing evidence, not ours.



## Upcoming — the focus epics' remaining stories, in order. Build their
## foundation now; invest nothing in what they condemn.

(the focus stories have no parent epic — attach them to their epics)
