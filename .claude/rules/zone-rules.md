---
paths:
  - "crates/kyzo-core/src/rules/**/*.rs"
---

# Zone: Rules — algorithms as rules

The invocable library: graph algorithms and utility rules.

## Required

- Every rule declares its determinism; all randomness flows from the one
  seed-reproducible generator — same seed, same output, every host.
- Inputs and outputs are relations. A rule touches storage and projections
  only through its typed graph/relation view, never directly.
- One algorithm per file, named for the algorithm, citing its source
  (paper or standard) in the module doc.
- Limits and options are typed and validated at construction.

## Forbidden

- Direct storage scans, projection internals, or session state.
- Unseeded or thread-timing-dependent randomness.
- A second in-memory graph representation — the one typed view serves all.
- Panics on any user-supplied option or degenerate graph shape.
