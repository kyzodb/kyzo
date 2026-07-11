---
paths:
  - "kyzo-crashfs/**/*.rs"
---

# Zone: Crashfs — the fault injector

A standalone instrument that makes the filesystem lie on command, so the crash
matrix in trials can prove the store survives it. Its nature dictates three
parts and no more: a plan, an application, a mount.

## Required

- Every fault decision is a pure function of the seed: the same seed reproduces
  the identical fault schedule, byte-for-byte, on every host and every run.
- Three parts, one nature — a fault PLAN (what fails and when, decided only by
  the seed), an APPLICATION (the passthrough filesystem that enacts the plan
  over a backing directory), and a MOUNT lifecycle (capability detection,
  setup, teardown).
- The instrument injects faults; it never judges outcomes. The verdict lives in
  `kyzo-trials`' crash matrix that drives this; a green run here means nothing
  by itself.
- Capability detection is explicit: where the platform cannot mount, the
  harness refuses with a typed reason — never a silent degrade to a no-op that
  reports success.

## Forbidden

- Any nondeterminism in fault selection — wall clock, thread timing, unseeded
  randomness. A fault that cannot be replayed from its seed is a defect.
- A dependency into the engine crates: the injector stands beside the
  filesystem, not inside the database. `kyzo-trials` depends on it; it depends
  on nothing of ours above the fault plane.
- `unwrap`/`expect` on any path reachable from a running campaign — a crashed
  injector is a lost counterexample, not a result.
