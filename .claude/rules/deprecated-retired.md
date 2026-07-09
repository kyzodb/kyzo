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
- **pointsto_repro.rs** — reproducer for the recursion-class memory defect;
  dies when the evaluator-rebuild re-measurement closes it. Its workload
  shape may graduate into the proving ground's recursive-Datalog rig.
- **fixpoint_mem_profile.rs** — allocation attribution for the same defect;
  dies with it. Attribution technique worth noting in the bench lane, not
  keeping as code.
- **oltp_mixed_profile.rs** — attribution instrument for the OLTP census in
  flight; dies when that census lands its routing.
- **tc_regress.rs** — differential reproducer for a closed defect class;
  its differential belongs to the oracle/trials machinery now — retire.
- **bulk_ingest_profile.rs** — ingest-time attribution; retire, with any
  standing measurement graduating to `benches/`.
- **hnsw_build_profile.rs** — superlinear-build chase; the RaBitQ-first bet
  reshapes the ground under it — retire, learnings to the vector story.
- **lsm_keyspace_policy_bench.rs** — existed to inform a decision that has
  been made; a decided decision's instrument is done — retire.
- **determinism_digest.rs** — the one graduate: seeded byte-identity probing
  is a standing public claim, so it ascends into `kyzo-trials`'s
  determinism campaign rather than dying.
