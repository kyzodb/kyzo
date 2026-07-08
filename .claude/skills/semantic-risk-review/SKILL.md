---
name: semantic-risk-review
description: For a diff touching a high-blast-radius zone (memcmp key encoding, the storage KV backend + time travel, query/Datalog semantics, or an FFI binding), dispatch the matching reviewer agent(s) and gate on their findings. Use before finalizing a change in those zones.
---

# Semantic risk review

Gate high-blast-radius changes on the right reviewer before finalizing.

## Trigger -> reviewer
| Diff touches | Dispatch agent |
|---|---|
| `data/value/{canonical,tag,number,row}.rs`, `data/bitemporal.rs` (on-disk formats) | `storage-semantics-reviewer` |
| `storage/**` (the KV backend + time travel) | `storage-semantics-reviewer` |
| `query/**` (Datalog engine) or `engines/**` (index-search operators) | `query-semantics-reviewer` |
| any `kyzo-lib-*` binding or `unsafe` | `unsafe-ffi-reviewer` |
| build/clippy/test output to interpret | `cargo-diagnostics-triager` |
| any diff minting or reshaping a public type | check it against the world model (`kyzo-core/src/lib.rs` crate docs) and rule 03: one name per concept, one concept per name, constructors prove invariants, nothing descends the enforcement ladder. A new/reshaped authority type gets its `@authority` block (`scripts/authority-graph.py` ratchets drift); run `scripts/smell-scan.sh` on proof code and classify every hit |

## Dispatch is operator-authorized
Spawning a reviewer is a sub-agent dispatch: describe what will be reviewed and by which agent,
and ASK the operator first (CLAUDE.md) — never fan out on your own decision. This makes the pass
no less mandatory: rule 00 gates high-blast-radius diffs on it, so a withheld authorization blocks
finalization, it does not waive the review.

## Tree discipline for reviewers
Reviewers are read-only against the real tree and verify in COPIES (rsync the working tree;
uncommitted work does not survive git-based copies). Every copy goes in a UNIQUELY-NAMED directory
under the reviewer's own scratchpad (e.g. `kyzo-<topic>-$RANDOM`) — a shared scratch path has
cross-contaminated reviews before. Prefer a FROZEN target: review against a named commit, and note
in the report if the tree moved mid-review. NEVER run git reset/checkout/clean/stash against the
real tree — that has destroyed uncommitted work here twice. Verification copies are not work
products and are deleted after the verdict.

## The gate is recursive
Fix waves responding to review findings are themselves unreviewed code —
they get their own hostile pass before anything is called resolved. Reviewer
briefs say REFUTE, name the specific claims to attack, and demand
CONFIRMED-vs-PLAUSIBLE verdicts with concrete failure scenarios.

## Both constitution lenses, not just bugs
A brief carries the whole review standard from CLAUDE.md: check for bullshit, AND flag missed
greatness — the avoided hard architectural choice, types not used as authority, accepted accidental
complexity, an ordinary-database pattern copied without asking whether KyzoDB's ordered substrate /
determinism / time / provenance model allows something better, or "works" where a sharper
engineering bet was available. Safe-looking code that lowers the engine's ceiling is a finding.

## Steps
1. Identify which zones the diff touches (`git diff --stat`).
2. Dispatch the matching reviewer agent(s) with the diff and paths.
3. Treat findings as gating: resolve or consciously accept each before proceeding.
4. Summarize the review outcome alongside the change.
