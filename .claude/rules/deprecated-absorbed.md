---
paths:
  - "kyzo-core/src/engines/hnsw_filter_harness.rs"
  - "kyzo-core/src/engines/gazetteer_hostile.rs"
  - "kyzo-core/src/engines/sparse_hostile.rs"
  - "kyzo-core/src/runtime/db_battery.rs"
---

# Absorbed — scaffolding and batteries dissolving into their proper homes

Standalone battery/harness files are structure masquerading as siblings of
the code they test; the target test ontology puts internal adversarial
tests in the module's test submodule, beside what they attack.

Per-file assessments emptied: the prior census was run under a defective
loop (sampled reads; the map misread as closed). Entries return as the
census re-runs under story #167's corrected ontology-first process.
