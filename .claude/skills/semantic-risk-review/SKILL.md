---
name: semantic-risk-review
description: For a diff touching a high-blast-radius zone (memcmp key encoding, the storage KV backend + time travel, query/Datalog semantics, or an FFI binding), dispatch the matching reviewer agent(s) and gate on their findings. Use before finalizing a change in those zones.
---

# Semantic risk review

Gate high-blast-radius changes on the right reviewer before finalizing.

## Trigger -> reviewer
| Diff touches | Dispatch agent |
|---|---|
| `data/{memcmp,tuple,bitemporal,fact_payload}.rs` (on-disk formats) | `storage-semantics-reviewer` |
| `storage/**` (the KV backend + time travel) | `storage-semantics-reviewer` |
| `query/**` (Datalog engine) or `engines/**` (index-search operators) | `query-semantics-reviewer` |
| any `kyzo-lib-*` binding or `unsafe` | `unsafe-ffi-reviewer` |
| build/clippy/test output to interpret | `cargo-diagnostics-triager` |
| any diff minting or reshaping a public type | check it against the world model (`kyzo-core/src/lib.rs` crate docs): one name per concept, one concept per name, constructors prove invariants, nothing descends the enforcement ladder |

## Tree discipline for reviewers and fix agents
Reviewers verify in COPIES (rsync the working tree; uncommitted work does
not survive git-based copies) so their builds and probe tests never race
the coordinator's. Every copy goes in a UNIQUELY-NAMED directory under the
reviewer's own scratchpad (e.g. `kyzo-<topic>-$RANDOM`) — two reviews have
been cross-contaminated by a shared scratch path; never reuse or share
one. Prefer a FROZEN target: ask the coordinator for a commit to review
against, and note in the report if the tree moved mid-review. NEVER run
git reset/checkout/clean/stash against the real tree — a hard reset has
already destroyed a concurrent agent's uncommitted work once (and a
single-file `git checkout` did it again later). Restoring the real tree
is the coordinator's job, never a reviewer's — and the coordinator
restores by re-applying edits, never by git-mutating uncommitted state.

CLAUDE.md's "one tree, one branch" governs WORK — edits land only in the
real tree; no agent worktrees or parallel patch stacks. Read-only
verification copies are not work products and are deleted after the
verdict.

## The gate is recursive
Fix waves responding to review findings are themselves unreviewed code —
they get their own hostile pass before anything is called resolved. Reviewer
briefs say REFUTE, name the specific claims to attack, and demand
CONFIRMED-vs-PLAUSIBLE verdicts with concrete failure scenarios.

## Steps
1. Identify which zones the diff touches (`git diff --stat`).
2. Dispatch the matching reviewer agent(s) with the diff and paths.
3. Treat findings as gating: resolve or consciously accept each before proceeding.
4. Summarize the review outcome alongside the change.
