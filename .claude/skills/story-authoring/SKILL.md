---
name: story-authoring
description: How a KyzoDB board story is written — the nine-section body, labels, the four-milestone board, and the refinement pass that makes the story command the work. Use when creating or refining a board issue.
---

# Story authoring

How a KyzoDB board story is written. The formal definition stays tight; comments are the working
dump. Every story gets: a label set, a milestone, a board Priority (the operator's product
decision), and (at start) its own branch wired to the issue via
`gh issue develop <n> --name story-<n>-<slug> --base main`.

## The format (issue body — the nine sections, in this order)

1. **Current code evidence** — what the tree actually contains today, with file/commit/result
   citations. Verified against the real code, never remembered.
2. **Architectural smell** — the principled statement of what is wrong or missing and why it
   lowers the engine's ceiling.
3. **Required invariant** — the law this story establishes or preserves; what must hold when it
   lands.
4. **Acceptance criteria** — the boundary. Checkable post-work facts only; this section is what
   the `scripts/story-evidence` comment answers.
5. **Non-goals** — law. What this story must NOT do, named to block scope creep and "optimize a
   little while we're here".
6. **Dependencies** — upstream (what must land first) and what this story feeds.
7. **Benchmarks / proofs** — the instruments that close it: gates, differentials, mutation proofs,
   measurements with the regression floor named. Nothing aspirational; everything checkable.
8. **Open decisions** — the genuinely undecided rulings, named so they are decided in-story instead
   of discovered mid-work.

**Hardest obligation** (closing section) — the failure mode this story most tempts, the invariant
against it, and the proof that shows the invariant held. Name the scariest thing so it cannot be
dodged.

## The refinement pass: make the story command the work

A story is not finished being written until "do this story" is a complete brief. Before execution
starts, study it against the real code (read the body AND every comment, since a comment can
supersede the body) and sharpen it until the text itself forces the hardest and best engineering:

- **Name the scariest thing and command it first, completely.** Find the one piece where wrong
  answers hide and there is no safety net (the on-disk format, a silent-wrong-answer path) and write
  its task as a direct order that builds it head-on and to completion before any easy win.
- **Every task is an order, not a description.** Imperative verbs; the concrete layout, algorithm,
  or type named; the `file:line` seams cited. No "consider", "try to", "should".
- **Prove-it gates inline.** Each hard task carries, on its own line, the proof that closes it
  (the differential green, the mutation killed, the byte-identical suite), so it cannot be called
  done unproven.
- **Committed decisions marked do-not-reopen.** State the rulings already made, and their rejected
  alternatives with reasons, as closed, so nobody re-litigates them.
- **Execution discipline, stated once.** One focused pass, hardest-first, no deferral, never
  stash/reset/checkout the tree, commit and push each green unit as it lands, flip that task's box
  in the same motion.
- **Refine, never invent.** Sharpen framing, ordering, and specificity only. Preserve every
  technical fact, ruling, and ref; fold a superseding comment into the body; never add scope or
  change a decision under cover of refining.

The finished test: hand the story over with no other words, and the hardest, most dangerous
engineering gets built first and proven, with nothing left to a coward's "later".

## The comments are the dumping ground

Design rulings, research reports, evidence, amendments, losing benchmark runs — all live as issue
comments, pinned by linking from the body when load-bearing. Rewriting the body to absorb a comment
is allowed only to keep the sections true; history stays in the comments.

## Labels (major work types)

`engine` `perf` `format` `oracle` `bench` `bindings` `trials` `demos` `devex` `infra` `hosting`
`authority` `product` `platform` `capabilities`. Every story carries `story` + its type labels;
reference/epic artifacts carry `epic`. `format` is special: it means the on-disk format is touched
and the full format gate (round-trip, order-embedding, corruption, FormatVersion decision,
storage-semantics hostile review) applies.

## Milestones

The four-milestone priority board: `Version 0.9.0` → `Version 1.0.0` → `Public Proof` →
`Future Roadmap`. A story's milestone and Priority are the operator's product decision — the
tooling (`scripts/board-context`) surfaces that order; it never re-derives or moves it.
