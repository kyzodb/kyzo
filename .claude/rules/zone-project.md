---
paths:
  - "kyzo-core/src/project/**/*.rs"
---

# Zone: Project — projections

Rebuildable speed over canonical truth. Never truth itself.

## Required

- The projection law is absolute: every structure here is rebuildable from
  canonical facts, byte-identically, at any time. If canonical storage cannot
  regenerate it, it is misplaced authority and a red gate.
- Projections serve CANDIDATES to exec's search operators; they never answer
  queries directly. Exactness and recall contracts are declared types.
- Maintenance is commit-coupled and generation-stamped; staleness is
  detectable by type, never silent.
- Deterministic construction and search: same facts, same seed, same
  projection, same candidates — on every host.
- Every engine follows the uniform shape: its maintenance, its search, its
  law, one directory per genuinely distinct projection kind.
- A decode failure crossing an engine boundary becomes a TYPED engine
  corruption error — a raw decode error never leaks where the contract says
  index corruption.
- Manifests and per-engine metadata carry config only, never value
  authority, through the store's one ruled metadata door.

## Forbidden

- Writing canonical state, ever, from anywhere in this zone.
- A projection consulted as a source of truth (existence, counts, or values).
- Scaffolding, harnesses, or hostile batteries shipped as engine siblings —
  batteries live in the module's test submodule; campaigns live in trials.
- Un-owned foreign code: every file in this zone carries our module doc and
  our type discipline, vendored lineage or not.
