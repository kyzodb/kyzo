---
paths:
  - "kyzo-core/examples/pointsto_repro.rs"
  - "kyzo-core/examples/fixpoint_mem_profile.rs"
  - "kyzo-core/examples/oltp_mixed_profile.rs"
  - "kyzo-core/examples/tc_regress.rs"
  - "kyzo-core/examples/bulk_ingest_profile.rs"
  - "kyzo-core/examples/hnsw_build_profile.rs"
  - "kyzo-core/examples/lsm_keyspace_policy_bench.rs"
  - "kyzo-core/examples/determinism_digest.rs"
  - "kyzo-core/examples/standing_smoke.rs"
  - "kyzo-core/examples/ra_determinism.rs"
  - "kyzo-core/examples/ra_profile.rs"
  - "kyzo-core/examples/bench_tc.rs"
---

# Retired — issue-pinned instruments that die with their issues

An instrument earns survival only by graduating: it becomes a permanent
bench (`benches/`), a trial (`kyzo-trials`), or it dies when its defect
closes. The tree keeps no museum of reproducers. Before deletion, harvest
anything load-bearing — a threshold, a workload shape, a measurement law —
into the owning story, bench, or trial.

Entries below are census-verified: each file's construct inventory was
enumerated to closure before its verdicts landed.

## examples/bulk_ingest_profile.rs (121 lines; inventory: module doc, 2
consts, 3 helpers, 6-phase main — closed)
- **L1:** no successor; retires when issue #74's routing lands. Every
  construct is Remove: a thin `main` over `bench_api` (a Sealed door, so
  its death is entailed twice); one concept (the #74 attribution
  instrument) at its natural size — no split.
- **L2:** harvest before deletion: the phase-decomposition workload shape
  (parse literal-vs-param / encode-only / probe-only-not-found /
  bare-fjall floor / full path ×{literal,param}×{fjall,mem} — each phase
  does one thing so times subtract cleanly) graduates to `benches/`; the
  finding that the SSI current-row probe is paid UNCONDITIONALLY per put
  row (floors bulk ingest regardless of other fixes) is #120 evidence,
  not code. `Backend::Mem` in its phases dies with SimStorage's move to
  trials.

## examples/determinism_digest.rs (213 lines; inventory: module doc incl.
merkle-exclusion doctrine, `no_params`, `hash_named_rows`, `run`, `main`
with graph/bitemporal/interval workloads + merkle print — closed)
- **L1:** the one graduate: public-surface byte-identity probing ascends
  into `kyzo-trials`' determinism campaign (seat exists:
  `kyzo-trials/determinism.rs`). One concept at its natural size.
- **L2:** preserve through the ascent: row ORDER hashed as part of the
  claim ("the right SET in the wrong ROW ORDER is exactly the bug");
  driver-varies-the-axes design (threads/repetition/architecture belong
  to the driver, the binary stays one execution); the honestly-named
  merkle-root exclusion (system-time keys legitimately differ run to run;
  true on-disk identity is blocked on an injectable clock — a named
  structural boundary, not a harness bug); per-query hash printing so CI
  names the diverging query. Arrival check: `COMBINED` rides
  `std::DefaultHasher`, unspecified across Rust releases — a published
  campaign artifact needs a pinned, named hash.

## examples/fixpoint_mem_profile.rs (224 lines; inventory: module doc,
counting `GlobalAlloc` (statics `LIVE_BYTES`/`PEAK_BYTES`/`ALLOC_CALLS`,
`bump`, `Counting`), `SEED`, `measure_peak`, `vm_hwm_kib`, `report`,
`main` with tc-chain + points_to sweeps + two env-gated extras — closed)
- **L1:** no successor; dies when #68's evaluator-rebuild re-measurement
  closes. All constructs Remove; the two workload shapes (single-
  occurrence tc/chain baseline vs the double-occurrence points_to
  self-join the AtomOccurrence fix targets) may graduate into the proving
  ground's recursion rigs.
- **L2:** the peak-live-heap technique (high-water of allocated−freed,
  tracker reset before the timed call so seed data is excluded) is
  bench-lane documentation, not code — and it cannot cross as code: the
  counting allocator is `unsafe impl GlobalAlloc` in an EXAMPLE, outside
  `lib.rs`'s `#![forbid(unsafe_code)]` and invisible to rule #2's greps
  as written (OPERATOR FLAG: the unsafe rule's mechanical check has an
  examples-shaped hole; this file and pointsto_repro.rs are the two
  occupants). Doc note worth keeping: the catastrophic Mem-backend
  scaling it once showed was a SEPARATE SimStorage::range_scan bug, since
  fixed — fjall is the path that matters.

## examples/hnsw_build_profile.rs (133 lines; inventory: module doc,
consts SEED/DIM, `splitmix64`, `next_f32`, `gen_vec`, `no_params`,
`put_script`, `run_one`, `main` with doubling sweep — closed)
- **L1:** no successor; the RaBitQ-first bet reshapes the ground under
  the superlinear-build chase — retire whole. Public-surface only (no
  sealed-door entanglement), so nothing else is implicated.
- **L2:** harvest: the measured curve (~O(n^1.5) at M=16/ef=200: 1k→1.85s,
  3k→9.85s, 10k→50.4s vs hnswlib ~34s/1M) is vector-story evidence; the
  fitted-exponent-between-doublings readout is the right scaling
  instrument for whatever replaces it; batched `:put` driving `hnsw_put`
  per row documents that backfill and incremental insert share one path.

## examples/lsm_keyspace_policy_bench.rs (209 lines; inventory: module
doc, `params`, `report`, `seed_points`, `phase_ingest`,
`phase_point_get`, `phase_full_scan`, `seed_dense_chains`,
`phase_asof_dense_chains`, `main` with tuned StorageOptions — closed)
- **L1:** no successor; it informed the #118 Monkey/Dostoevsky keyspace
  decision, which is made — a decided decision's instrument is done.
- **L2:** harvest to the bench lane: the four canonical LSM workload
  shapes (ingest, point-get, full-scan, as-of over dense version chains —
  the deep-level probe bitemporal storage uniquely creates); the
  shrunken-memtable trick (64 KiB units so modest rows span real levels);
  and its measurement law: numbers mean nothing alone, only as a
  before/after pair on the same tree.

## examples/oltp_mixed_profile.rs (137 lines; inventory: module doc with
the diagnosed mechanism, `params`, `report`, `seed`, `phase_read_only`,
`phase_mixed`, `main` sweeping rows∈{2k,5k,10k} — closed)
- **L1:** no successor; dies when the OLTP census (#82 line) lands its
  routing. All constructs Remove.
- **L2:** the diagnosis is the keeper: every committed write bumps the
  relation watermark, so every read-after-write missed the segment and
  `segment_at` rebuilt by FULL relation scan — O(n) per point read. The
  A/B design (identical ops; only watermark stillness varies) is the
  proof shape for the fix. Note: segments.rs has since landed the
  witness-by-signature + miss-gate discipline, so this instrument's
  pathology is the REGRESSION case `project/current.rs` must stay proven
  against, not an open bug.

## examples/pointsto_repro.rs (217 lines; inventory: module doc, counting
`GlobalAlloc` twin, SEED, `vm_hwm_kib`, `Phase`, `measure`, `report`,
`gen_rel`, `LOAD_CHUNK_ROWS`, `load_relation`, `POINTSTO_KZ` verbatim
program, `main` with full-scale env gate — closed)
- **L1:** no successor; dies when the evaluator-rebuild re-measurement
  closes #68-reopened. Workload shape may graduate to the proving
  ground's recursive-Datalog rig.
- **L2:** the lesson that outlives it: #68's closing claim was measured
  through `pub(crate)` seams while the PUBLIC `run_script` path still
  OOMed — this repro exists to force claims through the public surface,
  which is why trials-law makes campaigns public-surface-only. Second
  occupant of the unsafe-GlobalAlloc-in-examples hole (see
  fixpoint_mem_profile.rs). Byte-parity discipline with kyzo-bench
  (same generator algorithm, seed, chunking, program text diffed
  byte-for-byte) is the standard for any future cross-harness repro.

## examples/tc_regress.rs (173 lines; inventory: module doc, consts
SEED/N/M/LOAD_CHUNK_ROWS, `SplitMix64` (new/next_u64/below),
`vmhwm_kb`, `random_digraph`, `no_params`, `main` with
full/count/limit10 variants — closed)
- **L1:** no successor; differential reproducer for a closed defect
  class — its differential belongs to oracle/trials machinery now.
- **L2:** one reusable idea for the bench lane: the full/count/limit10
  trio runs the SAME fixpoint varying only what is materialized and
  returned, cleanly separating evaluation cost from result-surfacing
  cost. Its SplitMix64 duplicates kyzo-bench's BY DESIGN (byte-parity
  with the reported workload) — parity duplication, not drift.

## examples/standing_smoke.rs (76 lines; inventory: header, module doc
(independent end-to-end exercise of the PUBLIC standing-query surface,
"written from the outside as a user would — different query and data
than the in-tree tests"), no_params, and a print-driven main: register
an aggregating standing query, commit a new lower min, apply_pending +
print delta and recompute, then RETRACT the current min ("the hard
case: the aggregate must rescan the group... which no per-kind delta
formula can do"), print again — closed)
- **L1:** retire, superseded: `tests/standing_queries.rs` now drives
  the identical surface (including the min-retraction rescan, in a
  multi-commit drain) with ASSERTIONS instead of println eyeballs —
  the asserting form is strictly stronger, and the tree keeps no
  museum. Nothing to harvest that the test doesn't already carry.
- **L2:** nothing condemned beyond the supersession itself; the
  outside-in posture survives in the tests/ suite.

## examples/ra_determinism.rs (82 lines; inventory: header, module doc
(the cross-thread determinism probe: workloads that "actually
parallelize" hashed canonically, the DRIVER re-runs under
RAYON_NUM_THREADS ∈ {1,2,4,8} and diffs — "identical hashes at every
thread count == byte-identical output"), `hash_output` (Debug
serialization through DefaultHasher), and main (five bench_api
workloads across both backends, per-workload + combined hashes,
THREADS-stamped output line) — closed)
- **L1:** graduates → `kyzo-trials/src/determinism.rs`, the same lane
  determinism_digest's entry already ascends into — the two probes
  are one campaign (digest covers the public script surface, this one
  the parallelizing RA workloads); the bench_api dependency dissolves
  per the sealed door's entry when the workload rigs land in benches/.
- **L2:** preserve through the ascent: driver-varies-the-axes design
  (thread count belongs to the driver, the binary is one execution);
  per-workload hashes so a divergence NAMES its workload. Same
  arrival check as determinism_digest: DefaultHasher is unspecified
  across Rust releases — the campaign artifact needs a pinned hash.

## examples/ra_profile.rs (156 lines; inventory: header, module doc
("perf/valgrind are not available in the proving environment", so the
where-does-the-time-go question is answered by a counting global
allocator + wall time; timed region is evaluation only), the
`Counting` GlobalAlloc (calls + bytes; unsafe instrument outside the
crate-root forbid boundary — the pointsto_repro precedent),
`Measured`/`measure` (untimed warm-up paying "any one-time lazy
costs"), the aligned `row` printer with per-row allocation rate, and
main (scan_filter at two selectivities, three TC shapes, join3,
aggregation × both backends) — closed)
- **L1:** retire when #120's vectorization ascent closes (the same
  fate-class as fixpoint_mem_profile): its question — the per-row
  dispatch+allocation tax — is exactly what the ascent removes, and
  the surviving form of the claim is the committed bench-results
  rows, not the instrument. Harvest first: the per-row a/row metric
  and the warm-up-then-measure protocol into the bench lane.
- **L2:** nothing condemned; the allocation-churn framing is #120
  evidence vocabulary.

## examples/bench_tc.rs (149 lines; inventory: header, module doc (the
community-standard workload: "the canonical two-rule transitive-closure
program run over a REAL SNAP graph... We invent no data and no query";
everything through the public front door "so the identical file
measures any engine revision"; the machine-readable output line
documented), LOAD_CHUNK_ROWS, `peak_rss_kb` (VmHWM — "the honest
memory high-water mark"), `read_snap_edges` (comment-skipping SNAP
parser), and main (chunked load through :put; the full-vs-count
variants; timed load and query; the one-line TC record) — closed)
- **L1:** graduates → the bench lane as a permanent instrument: this
  binary PRODUCES the committed bench-results rows the engine's own
  defaults cite as evidence (runtime/db.rs's 50M derived-tuple
  ceiling is justified against tc/snap-p2p-Gnutella08's recorded
  numbers — deleting this instrument would orphan that ledger). It
  keeps its argv-driven example-binary form (criterion benches can't
  take a graph file argument); the fetch script and bench-results/
  are its ledger.
- **L2:** preserve verbatim: invent-nothing benchmarking (published
  graph, textbook program, public door); machine-readable one-line
  records; VmHWM as the honest memory number. Nothing condemned.
