---
paths:
  - "kyzo-oracle/**/*.rs"
---

# Zone: Oracle — the reference semantics

The engine's naive twin. The law here is INVERTED.

## Required

- Slow is correct: obviousness outranks performance, always. The value of this
  zone is that it is too simple to share a bug with the engine.
- Complete over the whole language: an `unsupported` answer is a coverage hole
  to close, never a boundary to accept. The oracle owns its OWN expression
  evaluator, its own temporal semantics, its own reference provenance.
- Small enough to hostile-review line by line.
- Checkers re-derive from scratch over the model; they never trust
  intermediate engine state.
- Depends on the model (the shared vocabulary) ONLY.

## Forbidden

- Optimization. A performance improvement here is a defect.
- Importing anything from the engine: no evaluator code, no execution
  currency, no clever algorithms — not even the ideas. The crate wall makes
  the import impossible; the review keeps the ideas out.
- Sharing helper code with what it judges.
