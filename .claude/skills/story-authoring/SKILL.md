# Story authoring

How a KyzoDB board story is written. The formal definition stays tight; comments are the working
dump. Every story gets: a label set, a milestone, and (at start) its own branch wired to the issue
via `gh issue develop <n> --name story-<n>-<slug> --base main`.

## The format (issue body — keep it to these three blocks)

**Telos** — one short paragraph: why this exists, and the *value state change* — what is true about
the engine/product after this story that was not true before. No mechanism talk here; outcome only.

**Tasks** — a checklist ordered **hardest-first**, with the single hardest item marked `**(hardest)**`
and one clause on *why* it is the hardest (where wrong answers hide, where there is no safety net,
where a format can corrupt). Before ordering, ask: "is this dependency order or comfort order?"
Dependency order may override hardest-first; comfort order never does.

**Definition of Done** — post-work validation only: the gates, differentials, mutation proofs,
benchmark instruments (with the regression floor named), and review passes that must be green.
Nothing aspirational; everything checkable.

## The refinement pass: make the story command the work

A story is not finished being written until "do this story" is a complete brief. Before it goes to a
builder, study it against the real code (read the body AND every comment, since a comment can
supersede the body) and sharpen it until the text itself forces the hardest and best engineering:

- **Name the scariest thing and command it first, completely.** Find the one piece where wrong
  answers hide and there is no safety net (manual unsafe memory, the on-disk format, a
  silent-wrong-answer path) and write its task as a direct order that builds it head-on and to
  completion before any easy win: "do this first and completely; the easy parts wait behind it, and
  it is proven before you move on."
- **Every task is an order, not a description.** Imperative verbs; the concrete layout, algorithm, or
  type named; the `file:line` seams cited. No "consider", "try to", "should".
- **Prove-it gates inline.** Each hard task carries, on its own line, the proof that closes it (Miri
  zero findings, the differential green, the static assertion compiled in, the byte-identical suite),
  so it cannot be called done unproven.
- **Committed decisions marked do-not-reopen.** State the rulings already made, and their rejected
  alternatives with reasons, as closed, so no builder re-litigates them and burns a session.
- **Execution discipline, stated once.** One focused pass, hardest-first, no chunk-crawl, no deferral,
  never stash/reset/checkout the tree, commit each green unit as an unwind point, flip that task's box
  as it lands, push nothing.
- **Refine, never invent.** Sharpen framing, ordering, and specificity only. Preserve every technical
  fact, ruling, and ref; fold a superseding comment into the body; never add scope or change a
  decision under cover of refining.

The finished test: hand the story to a builder with no other words, and the hardest, most dangerous
engineering gets built first and proven, with nothing left to a coward's "later".

## The comments are the dumping ground

Design rulings, research reports, evidence, amendments, losing benchmark runs — all live as issue
comments, pinned by linking from the body when load-bearing. Rewriting the body to absorb a comment
is allowed only to keep the three blocks true; history stays in the comments.

## Labels (major work types)

`engine` `perf` `format` `oracle` `bench` `bindings` `trials` `demos` `devex` `infra` `hosting`
`story`. Every story carries `story` + its type labels. `format` is special: it means the on-disk
format is touched and the full format gate (round-trip, order-embedding, corruption, FormatVersion
decision, storage-semantics hostile review) applies.

## Milestones

`Engine v1.0` — everything that must seal before the engine is declared done and binding work
starts. `Benchmarks` — the kyzo-bench comparative lane. Bindings get their own milestone when that
phase opens.
