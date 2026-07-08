---
name: story-execution
description: The discipline for executing one story from the KyzoDB board. Use when picking up any story. Carries the story lifecycle (start, branch, checkboxes, evidence, close); defers to .claude/rules/ for gates, no-deferral, reporting, and type law.
---

# Story execution

This is our own engine, not a migration: every story is an architectural move toward the greatest
possible engine, judged by the invariant it lands, never by what any prior codebase did. The board
(org project KyzoDB Work) is the plan; `.claude/rules/` are the law — 00 gates, 01 no-deferral,
02 final report, 03 type-driven construction. This skill carries only the story lifecycle. It never
restates law; where it seems to conflict with a rule, the rule wins.

## The mantra — chant it before every piece of new code
**Do the work. Prove the work. Tell the truth about the work.** The tells: relief means escaping;
narrating means lying; defending before re-examining means inverting; converging to the last thing
said means the world model is lost. Appearance is the enemy; reality is the only client.

## How a story starts
1. **The operator starts it** by moving the `active-story` label; `scripts/board-context` then
   injects `.claude/active-story.md` (body verbatim + recent comments) into every prompt. Never
   self-select work: `.claude/next-work.md` is context, not permission — noticing future work is
   fine; silently implementing it is not.
2. **Work on the story's branch** (`story-<n>-<slug>`, wired to the issue). One tree, one branch —
   no worktrees, no parallel patch stacks.
3. **Read the body AND every comment** — a comment can supersede the body. The Acceptance section
   is the boundary; the Non-goals section is law. Do the story's stated scope and nothing else.

## Executing
4. **Hardest-first.** Before ordering tasks ask "is this dependency order or comfort order?" — the
   hardest item startable now comes first. Dependency order may override hardest-first; comfort
   order never does. Picking ripe work before hard work is deferral in costume (rule 01).
5. **One coherent target, max energy.** Every file lands in its exact end-state form; never land
   anything "to refactor later", never park a half-built middle — the moment code touches the repo
   it is the product. Prior art is a dead reference, never a design authority: interrogate every
   construct (*is this the best way, does it even belong?*) and land
   only the best version. "Battle-tested" is not a defense — five real defects hid behind it in the
   storage kernel. Between competing designs the better engine wins, even at the cost of rework; a
   risky design is acceptable when it is explicit, testable, reversible, and signal-bearing.
6. **Types are the ontology** (rule 03). The type graph is the system's world model (crate docs in
   `kyzo-core/src/lib.rs`); mint every type against the whole of it, never against one file's
   convenience. Push every invariant up the ladder **compiler > constructor > test**; never let one
   descend. New authority types get `@authority` blocks — the authority-graph gate ratchets drift.
7. **Verify, never assert** (the verify-with-build skill). Every build/test/bench through the
   container; every claim backed by a real run or a read of the file.
8. **Red-green-commit-push.** build → test → red? fix → green? commit → push → next. Never advance
   on red; the shared tree never sits unbuildable in any feature config or workspace member.
   Public/irreversible acts (merge to main, tags, releases, new remotes) wait for an explicit go;
   routine branch pushes do not.
9. **Adversarial review after committed-green.** Hostile review attacks the committed state, briefed
   to REFUTE; findings reopen their unit and fixes get their own build→test→commit. Self-verification
   covers mechanical claims only; semantic claims are contested territory until attacked.
   **Sub-agents only on operator authorization** — describe the proposed dispatch and ASK; never
   fan out on your own decision.

## Keeping the tracker truthful
- Board Status is the operator's; the tooling never moves cards. If the board and reality disagree,
  say so (`.claude/board-signal.md` surfaces it) — never silently "fix" the board.
- **Check off each issue task checkbox in the same motion that completes it**
  (`gh issue edit <n> --repo kyzodb/kyzo`) — built, tested, green, committed — so the issue reads
  N/M truthfully as work lands, not 0/M until the end. A box that bundles several steps is checked
  only when ALL of it is done; a checked box is a claim verified against the tree, never
  self-certified.

## Closing
- A story is done only when its Acceptance section is met and verified and the rule-00 gates are
  green — a gate not run means the story is not complete.
- **Evidence, not chat:** post completion evidence with
  `scripts/story-evidence --issue N [--gate-report ...] [--tests ...] [--acceptance ...]` — the one
  place tooling writes to GitHub, and it writes a comment, never board state. The story moves
  forward on that comment; the final report follows rule 02.
