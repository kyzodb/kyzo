---
paths:
  - "crates/kyzo-trials/**/*.rs"
---

# Zone: Trials — the campaigns

Attacks on published claims, rerunnable by strangers.

## Required

- Every trial attacks a PUBLIC claim through the public surface with published
  seeds — a stranger can rerun everything from the committed artifacts.
- The verdict space is binary: zero counterexamples, or the claim retracts.
  There is no "mostly passed."
- Corpora, harnesses, and seeds are versioned artifacts; a trial that cannot
  be reproduced proves nothing.
- Determinism trials compare bytes, not summaries. Differential trials judge
  against the oracle, never against the engine's own second opinion.
- The storage conformance kit is public: any backend (ours or a stranger's)
  can run it unmodified.
- Every expected value is independently derived — hand derivation, an
  independent test-only encoder, or a byte-by-byte comment — never captured
  from the engine's own output.

## Forbidden

- Testing internals — that is the job of unit/hostile tests living beside the
  code inside its authority. A trial needing a private door means the public
  surface is missing something; fix the surface.
- Weakened assertions to keep a campaign green; goldens copied from output.
- A trial whose failure is survivable narrative — a red trial blocks the claim
  it attacks, mechanically.
