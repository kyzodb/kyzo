---
name: story-execution
description: The discipline for executing one story from the KyzoDB board. Use when picking up any story, BEFORE the first edit. Forces the plan of attack (hardest task named, ontology delta, proof per task) and types-before-mechanism; defers to .claude/rules/ for all law.
---

# Story execution

This skill fires at the moment of pickup — the first ten minutes, where the real failures happen
and no gate can catch them. All law lives in `.claude/rules/` (00 gates, 01 no-deferral, 02 final
report, 03 type-driven construction); nothing here restates it.

## The mantra — chant it before every piece of new code
**Do the work. Prove the work. Tell the truth about the work.** The tells: relief means escaping;
narrating means lying; defending before re-examining means inverting; converging to the last thing
said means the world model is lost. Appearance is the enemy; reality is the only client.

## The opening move: the plan of attack, before the first edit
Write it down, in chat, at pickup — and it binds:

1. **The hardest task, and why it is the hardest** — where wrong answers hide, where there is no
   safety net. Answer the story's own Hardest obligation section explicitly.
2. **The ontology delta** — every type this story mints or reshapes, named before any code exists.
3. **The proof that closes each task** — the test, differential, mutation kill, or measurement.

The first commit goes at the hardest task. If it can't, the plan names the dependency that forces
the order — "warm up on the easy part" is deferral in costume, and the written plan is what makes
it visible. Nothing else in the stack enforces hardest-first mechanically: the commits either went
where the plan said the danger was, or they didn't.

## Types land first
Design and commit the story's types and authorities before the mechanism that uses them. A function
that dispatches on data gets its enum/newtype before its match arm exists (rule 03); new authority
types get their `@authority` block as they are minted, not at cleanup. A story that ends with "the
type comes later" did not do the story — the procedural version passing the gate is how type
avoidance survives.

## Lifecycle
- The operator starts a story by moving the `active-story` label; `.claude/active-story.md` is
  injected every prompt. Never self-select from `.claude/next-work.md`.
- Work on the story branch (`story-<n>-<slug>`). One tree, one branch.
- Read the body AND every comment — a comment can supersede the body. Acceptance is the boundary;
  Non-goals are law.
- Red-green-commit-push per unit; never advance on red. Public/irreversible acts wait for a go.
- Flip each issue task checkbox in the same motion its task completes (`gh issue edit`); a box
  bundling several steps is checked only when ALL of it is done.
- Sub-agents only on operator authorization — propose the dispatch and ask.
- Close on evidence: gates green (rule 00), then `scripts/story-evidence --issue N ...` posts the
  completion comment; the final report follows rule 02.
