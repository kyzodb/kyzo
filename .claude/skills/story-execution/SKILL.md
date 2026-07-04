---
name: story-execution
description: The discipline for executing one migration story from the KyzoDB board. Use when picking up any story. Enforces working from the board, one coherent end-state target, verify-with-build/test, and the anti-avoidance rules.
---

# Story execution

The migration is a sequence of stories on the board (see `REFACTOR.md`). Execute exactly one at a time,
in dependency order.

## The mantra — chant it before every piece of new code
**Do the work. Prove the work. Tell the truth about the work.** The tells: relief means escaping;
narrating means lying; defending before re-examining means inverting; converging to the last thing said
means the world model is lost. Appearance is the enemy; reality is the only client.

## Steps
1. **Move the story to In Progress** on the board before any work starts.
   Sequence the work **hardest-first**: before ordering tasks, ask "is this
   dependency order or comfort order?" — the hardest item startable now
   comes first. Picking ripe work before hard work is the drift that killed
   the original plans twice.
2. **Read the story and `REFACTOR.md`.** Do the story's stated scope and nothing else. Do not invent
   scope; do not start non-story work without saying so.
3. **One coherent target, max energy.** Every file lands in its exact end-state form. Never land
   anything "to refactor later", never manage a half-migrated middle: the moment code touches the repo
   it is the product. Copying from upstream is allowed; **blind copying is not** — interrogate every
   construct (*is this the best way, does it even belong?*) and land only the best version. Do the hard
   work first, not last. "Battle-tested" is not a defense: the storage kernel found five real defects
   hiding behind it.
4. **Types are the ontology.** The type graph is the system's world model (see the crate docs in
   `kyzo-core/src/lib.rs`); mint every type against the whole of it, never against one file's
   convenience. Work the questions in order: what is this *for* (telos) → what exists (substances) →
   essence vs accident → kind and differentia → composition → who may construct it, proving what →
   what form makes it valid → how it lawfully changes → what relates to what → what must be
   unrepresentable → does the whole carve reality at its joints. Push every invariant up the
   enforcement ladder: **compiler > constructor > test**; never let one descend.
5. **Verify, never assert.** Back every claim about the code or a change with a real `cargo build` /
   `cargo test` / run, or by reading the file. No conclusions from memory.
6. **Commit on green; review is a later phase (red-green-commit).** The cycle
   per build unit: **build → test → red? fix → green? COMMIT (local, never
   push) → next.** A commit is an unwind point, not a seal — unpushed, so
   `git reset`/`revert` fixes anything. NEVER advance on red; NEVER let the
   shared tree accumulate a giant uncommitted parallel-edit soup (that soup
   is what makes a full-suite run measure nobody's real state — it is the
   number-one source of integration waste). Commit each unit's OWN files as
   they go green (`git commit <paths>`), leaving other builders' in-flight
   work untouched. Do NOT run convoy-wide verification while a builder holds
   shared-dependency files mid-edit — the tree is not quiescent, so the
   result is noise. Hostile review and deeper architecture bug-hunting are a
   SEPARATE PHASE that begins only after ALL of a milestone's build work is
   committed-green and every build-caught bug is fixed. Nothing is PUSHED
   without an explicit maintainer go — push stays gated even though commits
   flow freely.
7. **The review phase still refutes.** When the build phase is done and the
   milestone is committed-green, the review/arch-hunt phase attacks it:
   adversarial reviewers briefed to REFUTE, on the committed state (a stable
   target, not a moving tree). Findings reopen their unit; fixes-of-findings
   get their own build→test→commit-on-green. An agent's self-verification
   covers mechanical claims only; semantic claims are
   contested territory until attacked. The center's code gets the same
   suspicion — the reviewers have caught it repeatedly.
8. **Deferral is sabotage unless blocked.** Work may leave the current story
   only with a named hard technical blocker (a true dependency, not
   difficulty, size, or tidiness). "Queued with reasons" and "follow-up
   story" are this project's number-one smuggling route for avoidance.
9. **Do not narrow scope to look done.** Whole-workspace, or say it is partial. Bindings are committed
   work, not deferrable; name hard work plainly instead of smuggling avoidance into a recommendation.
10. **Honor the DoD.** A story is done only when its Definition of Done is met and verified.
11. **Nothing public without a go.** Pushes and published packages wait for an explicit go from the
   maintainer.

## Dependency order
Storage kernel (#2) -> engine (#3) -> product green (#4); every binding story depends on #4. Go (#11)
additionally needs the C binding (#5), Clojure (#12) needs Java (#7), the Python client (#14) needs
Python (#6).
