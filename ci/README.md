# ci/ — recorded gate baselines

Baselines for the ratchet gates. Each is recorded in a reviewed commit at its
activation point, never invented before the code it measures exists:

- `unsafe-baseline.txt` — count of `unsafe` tokens in `kyzo-core/src` + `kyzo-bin/src`
  (recorded when engine sources first land; enforced by `scripts/check-unsafe.sh`). Growth requires an
  unsafe-invariants review and a deliberate baseline bump in the same PR.
- `coverage-baseline.txt` — workspace line-coverage percentage (recorded when the workspace first goes green;
  enforced by `scripts/check-coverage.sh`). Coverage may never drop below it.

Neither file exists yet by design: the gates fail or report loudly until the baselines are
recorded at their activation points. Do not add placeholder values.
