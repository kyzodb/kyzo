# ci/ — recorded gate baselines

Baselines for the ratchet gates. Each is recorded in a reviewed commit at its
activation point, never invented before the code it measures exists:

- `coverage-baseline.txt` — workspace line-coverage percentage (recorded when the workspace first goes green;
  enforced by `scripts/check-coverage.sh`). Coverage may never drop below it. It does not exist yet by
  design: that gate reports loudly until the workspace first goes green. Do not add placeholder values.

(The unsafe gate needs no baseline file: `#![forbid(unsafe_code)]` in every engine crate root makes
zero-unsafe a compile-time guarantee, checked by `scripts/check-unsafe.sh`.)
