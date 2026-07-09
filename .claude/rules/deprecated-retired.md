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

An instrument earns survival only by graduating: it becomes a permanent
bench (`benches/`), a trial (`kyzo-trials`), or it dies when its defect
closes. The tree keeps no museum of reproducers. Before deletion, harvest
anything load-bearing — a threshold, a workload shape, a measurement law —
into the owning story, bench, or trial.

Per-file assessments emptied: the prior census was run under a defective
loop (sampled reads; the map misread as closed). Entries return as the
census re-runs under story #167's corrected ontology-first process.
