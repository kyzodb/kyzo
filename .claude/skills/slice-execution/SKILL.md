---
name: slice-execution
description: The discipline for executing one migration slice from the KyzoDB board. Use when picking up any Slice N. Enforces working from the board, one coherent target, verify-with-build/test, and the anti-avoidance rules.
---

# Slice execution

The migration is a sequence of numbered slices on the board (see `REFACTOR.md`). Execute exactly one at a
time.

## Steps
1. **Read the slice issue and `REFACTOR.md`.** Do the slice's stated scope and nothing else. Do not
   invent scope; do not start non-slice work without saying so.
2. **One coherent target.** Align to the slice's end state; do not manage a half-migrated middle.
3. **Verify, never assert.** Back every claim about the code or a change with a real `cargo build` /
   `cargo test` / run, or by reading the file. No conclusions from memory.
4. **Do not narrow scope to look done.** Whole-workspace, or say it is partial. Bindings are committed
   work, not deferrable; name hard work plainly instead of smuggling avoidance into a recommendation.
5. **Honor the DoD.** A slice is done only when its Definition of Done is met and verified (Slice 3's DoD
   is a green `cargo build` + `cargo test`).
6. **Nothing public without a go.** Draft and show; commits to `main`, pushes, and published packages
   wait for an explicit go from the maintainer.

## Dependencies
Slice 0 -> 1 -> 2 -> 3 in order; every binding slice depends on Slice 3 (green core); Go depends on
Slice 4, Clojure on Slice 6, the Python client on Slice 5.
