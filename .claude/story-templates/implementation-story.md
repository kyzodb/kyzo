<!-- Copyright 2026, The KyzoDB Authors. MPL-2.0. -->
<!-- KIND: engine-implementation (label `engine` or an `Engine …` milestone). -->
<!-- story-contract-check requires every heading below plus the Failure mode / Proof lines. -->

One-sentence telos: the single behavior this story makes true, stated as an engine change.

**Shared authority types — see the map in #136.** Name which authority nodes this story
creates/consumes. Do not reinvent local wording for a shared type.

## 1. Current code evidence
Cite the current tree with `file:line` / `symbol`. What exists TODAY, verified — not a stale
pre-refactor claim. If a thing does not exist, say "grep finds none".

## 2. Architectural smell
The concrete defect in the current code this story removes. One paragraph, no theatre.

## 3. Required invariant
The typed invariant that must hold after this story. Prefer "impossible states unrepresentable"
phrasing (an authority you cannot forge, a reader that cannot write).

## 4. Acceptance criteria
Checkable outcomes. Each is a thing a reviewer can confirm from the diff or a test.

## 5. Non-goals
What this story explicitly does NOT do. Non-goals are law — they bound the blast radius.

## 6. Dependencies
Upstream stories/authorities consumed; downstream stories this unblocks. Use `#NNN`.

## 7. Benchmarks / proofs
The proofs that close this story: oracle differentials, laws, corruption corpora, and the bench-lane
measurement if performance is touched (perf claims close on the bench lane's re-measurement).

## 8. Open decisions
Genuinely-open rulings with: dataset / oracle / baseline / tolerance / reason. No invented
thresholds smuggled in as settled.

## Hardest obligation
Failure mode: the highest-value thing most tempting to defer, stated as a concrete failure.
Invariant: the typed guarantee that forecloses it.
Proof: the specific gate that catches it.
