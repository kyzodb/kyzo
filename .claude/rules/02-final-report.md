# Final Report Format

Final reports are factual. No narrative substitutes for a gate result.

## Required

- commits (range)
- exact commands run
- test counts (pass/fail)
- ignored/skipped tests
- clippy/fmt status (own code vs vendored, separated)
- benchmark status (with baseline commit + measurement)
- compile-fail status
- feature-config status (both)
- remaining red ledger (`01-no-deferral.md` shape)
- changed public semantics
- changed storage formats (+ FormatVersion decision)
- new authority doors
- deleted old doors

## Forbidden

- "massive progress" without gate status
- "mostly clean"
- "root-caused" without measurement
- "remaining cleanup" without a ledger
- "success" before gates pass
- a rhetorical confession standing in for a fix

## Claims close on external verification

A perf/regression claim opened by the bench lane closes on their re-measurement of the merged fix,
never on our own numbers. A claim can be true and untestable — then say so in the code, or build the
observability that makes it testable (prefer the latter). Comments are public claims: a stale doc
comment is a false statement to every future reader, a defect class fixed on sight.
