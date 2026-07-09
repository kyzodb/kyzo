---
paths:
  - "vendor/**/*.rs"
  - "vendor/**/Cargo.toml"
---

# Zone: Vendor — the owned storage fork

The fjall/lsm-tree fork: upstream code we deliberately rule.

## Required

- Upstream lineage preserved verbatim: license headers, attribution, and
  copyright stay untouched alongside ours.
- Every edit of ours is a deliberate engine decision, documented at the edit
  site with why the engine needs it — an undocumented divergence from
  upstream is a defect.
- Edits serve the storage contract above (ordered scans, SSI, consuming
  commits, crash safety); the fork evolves toward the engine's needs, never
  toward generic upstream parity.

## Forbidden

- A dependency from vendor back into any crate of ours — the fork stands
  below everything.
- Pulling upstream changes wholesale without re-justifying each against our
  edits.
- Treating vendor as unownable: it is first-party responsibility with
  third-party history.
