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
---

# Retired — issue-pinned instruments that die with their issues

Guidance grade: high-level review by smell/feel against the target purity
state. An instrument earns survival only by graduating: it becomes a
permanent bench (`benches/`), a trial (`kyzo-trials`), or it dies when its
defect closes. The tree keeps no museum of reproducers.

## The retirement law (applies to every file below)
- **L1:** no successor file. Each dies when its pinned issue closes, unless
  it graduates per below.
- **L2:** before deletion, harvest anything load-bearing — a threshold, a
  workload shape, a measurement law — into the owning story, bench, or
  trial. Code gets no tenure for having once been useful.

## Per-file fates
- **pointsto_repro.rs** — reproducer for the recursion-class memory defect
  (#68 reopened); dies when the evaluator-rebuild re-measurement closes it.
  Its workload shape may graduate into the proving ground's recursive-
  Datalog rig. Carries a lesson that outlives it: #68's closing claim was
  measured through `pub(crate)` seams while the PUBLIC `run_script` path
  still OOMed — the repro exists to force the claim through the public
  surface, which is exactly why trials-law makes campaigns public-surface-
  only. Second example carrying an `unsafe impl GlobalAlloc` (same
  operator-reported gap as fixpoint_mem_profile.rs).
- **fixpoint_mem_profile.rs** — peak-live-heap attribution for the same
  defect (high-water mark of allocated−freed, tracker reset before the
  timed call so seed data is excluded); dies with it. The technique is
  bench-lane documentation, not code — and it cannot cross anyway: the
  counting allocator is `unsafe impl GlobalAlloc`, which sits in an example
  file today, OUTSIDE `lib.rs`'s `#![forbid(unsafe_code)]` and invisible to
  the unsafe rule's greps as written (reported to the operator). Its two
  workload shapes (single-occurrence tc/chain baseline vs the double-
  occurrence points_to self-join that the AtomOccurrence-keyed delta fix
  targets) may graduate into the proving ground's recursion rigs.
- **oltp_mixed_profile.rs** — attribution instrument for the OLTP census in
  flight; dies when that census lands its routing. Its diagnosis is the
  keeper: every committed write bumps the relation watermark, so every
  read-after-write misses the current-state segment and `segment_at`
  rebuilds by FULL relation scan — O(n) to answer one point row. That
  pathology is the residency-discipline requirement `project/current.rs`
  must be built against (the A/B design — identical ops, only watermark
  stillness varies — is the proof shape for the fix).
- **tc_regress.rs** — differential reproducer for a closed defect class;
  its differential belongs to the oracle/trials machinery now — retire.
  One reusable idea for the bench lane: the full/count/limit10 variant trio
  runs the SAME fixpoint while varying only what is materialized and
  returned, cleanly separating evaluation cost from result-surfacing cost.
- **bulk_ingest_profile.rs** — issue #74 ingest attribution; a thin `main`
  over `bench_api` (sealed door), so its death is entailed twice. Retire;
  two harvests before deletion: the phase-decomposition workload shape
  (parse literal-vs-param / encode-only / probe-only / bare-fjall floor,
  times subtract cleanly) graduates to `benches/`, and the finding that the
  SSI current-row probe is paid unconditionally per put row (it floors bulk
  ingest regardless of other fixes) is performance-story evidence, not code.
- **hnsw_build_profile.rs** — superlinear-build chase; the RaBitQ-first bet
  reshapes the ground under it — retire. Harvest: the measured curve
  (~O(n^1.5) at M=16/ef=200: 1k→1.85s, 3k→9.85s, 10k→50.4s, vs hnswlib ~34s
  for 1M) is vector-story evidence, and the fitted-exponent-between-
  doublings readout is the right scaling-law instrument for whatever
  replaces it. Public-surface only — no sealed-door entanglement.
- **lsm_keyspace_policy_bench.rs** — informed the #118 Monkey/Dostoevsky
  keyspace-policy decision, which is made — retire. Harvest to the bench
  lane: the four canonical LSM workload shapes (ingest, point-get,
  full-scan, as-of over dense version chains — the last is the deep-level
  probe bitemporal storage uniquely creates), the shrunken-memtable trick
  (64 KiB flush units so modest rows span real levels), and its measurement
  law: numbers mean nothing alone, only as a before/after pair on the same
  tree.
- **determinism_digest.rs** — the one graduate: public-surface byte-identity
  probing (mutation history on real fjall, as-of reads, Interval; row ORDER
  hashed as part of the claim) ascends into `kyzo-trials`'s determinism
  campaign. Gold that must survive the ascent: the driver-varies-the-axes
  design (threads/repetition/architecture belong to the driver, the binary
  stays one simple execution) and the honestly-named merkle-root exclusion
  (system-time keys legitimately differ run-to-run; a true on-disk-identity
  claim is blocked on an injectable clock — a named structural boundary,
  not a harness bug). Arrival check: the digest rides `std::DefaultHasher`,
  whose algorithm is not specified across Rust releases — a published
  campaign artifact needs a pinned, named hash.
