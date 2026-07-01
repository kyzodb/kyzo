---
name: story-execution
description: The discipline for executing one migration story from the KyzoDB board. Use when picking up any story. Enforces working from the board, one coherent end-state target, verify-with-build/test, and the anti-avoidance rules.
---

# Story execution

The migration is a sequence of stories on the board (see `REFACTOR.md`). Execute exactly one at a time,
in dependency order.

## Steps
1. **Move the story to In Progress** on the board before any work starts.
2. **Read the story and `REFACTOR.md`.** Do the story's stated scope and nothing else. Do not invent
   scope; do not start non-story work without saying so.
3. **One coherent target, max energy.** Every file lands in its exact end-state form. Never land
   anything "to refactor later", never manage a half-migrated middle: the moment code touches the repo
   it is the product. Copying from upstream is allowed; **blind copying is not** — interrogate every
   construct (*is this the best way, does it even belong?*) and land only the best version. Do the hard
   work first, not last. "Battle-tested" is not a defense: the storage kernel found five real defects
   hiding behind it.
4. **Verify, never assert.** Back every claim about the code or a change with a real `cargo build` /
   `cargo test` / run, or by reading the file. No conclusions from memory.
5. **Do not narrow scope to look done.** Whole-workspace, or say it is partial. Bindings are committed
   work, not deferrable; name hard work plainly instead of smuggling avoidance into a recommendation.
6. **Honor the DoD.** A story is done only when its Definition of Done is met and verified.
7. **Nothing public without a go.** Pushes and published packages wait for an explicit go from the
   maintainer.

## Dependency order
Storage kernel (#2) -> engine (#3) -> product green (#4); every binding story depends on #4. Go (#11)
additionally needs the C binding (#5), Clojure (#12) needs Java (#7), the Python client (#14) needs
Python (#6).
