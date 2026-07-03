---
name: semantic-risk-review
description: For a diff touching a high-blast-radius zone (memcmp key encoding, the storage KV backend + time travel, query/Datalog semantics, or an FFI binding), dispatch the matching reviewer agent(s) and gate on their findings. Use before finalizing a change in those zones.
---

# Semantic risk review

Gate high-blast-radius changes on the right reviewer before finalizing.

## Trigger -> reviewer
| Diff touches | Dispatch agent |
|---|---|
| `data/memcmp.rs`, `data/tuple.rs` (key encoding) | `storage-semantics-reviewer` |
| `storage/**` (the KV backend + time travel) | `storage-semantics-reviewer` |
| `query/**` (Datalog engine) | `query-semantics-reviewer` |
| any `kyzo-lib-*` binding or `unsafe` | `unsafe-ffi-reviewer` |
| build/clippy/test output to interpret | `cargo-diagnostics-triager` |
| any diff minting or reshaping a public type | check it against the world model (`kyzo-core/src/lib.rs` crate docs): one name per concept, one concept per name, constructors prove invariants, nothing descends the enforcement ladder |

## Tree discipline for reviewers and fix agents
Reviewers verify in COPIES (rsync the working tree; uncommitted work does
not survive git-based copies). NEVER run git reset/checkout/clean/stash
against the real tree — a hard reset has already destroyed a concurrent
agent's uncommitted work once. Restoring the real tree is the
coordinator's job, never a reviewer's.

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
