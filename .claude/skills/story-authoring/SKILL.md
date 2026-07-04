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
