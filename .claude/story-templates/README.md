<!-- Copyright 2026, The KyzoDB Authors. MPL-2.0. -->
# Story-kind templates (#140)

The board is the work authority. Every issue is one KIND, and `scripts/story-contract-check`
resolves an issue's labels + milestone to exactly one kind and enforces only that kind's template.
Do not force an engine contract onto a reference artifact, and do not let an active engine story
skip the full contract.

| Kind | Signal (label / milestone) | Template | Enforced sections |
|---|---|---|---|
| engine-implementation | `engine` **or** an `Engine …` milestone (and not the kinds above it) | `implementation-story.md` | full 8-part contract + Hardest obligation |
| platform-story | `platform` or `infra` (with `story`) | `product-platform-spike.md` | Purpose · Scope/Required design · Acceptance · Hardest obligation |
| product-spike | `product` | `product-platform-spike.md` | substance floor |
| proof | `benchmark` / `trial` / `demo` / `public-proof` | `benchmark-trial-demo.md` | substance floor |
| reference | `reference` | `reference-artifact.md` | substance floor |
| epic | `epic` | `epic-roadmap.md` | substance floor |
| generic-story | `story` only | `implementation-story.md` (light) | Acceptance · Non-goals · Hardest obligation |

Kind precedence (first match wins): reference → epic → product → proof → platform/infra →
engine/Engine-milestone → story. The precedence is in `scripts/story-contract-check`; these
templates are the human-readable canon it enforces.
