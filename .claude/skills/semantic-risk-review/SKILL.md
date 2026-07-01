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

## Steps
1. Identify which zones the diff touches (`git diff --stat`).
2. Dispatch the matching reviewer agent(s) with the diff and paths.
3. Treat findings as gating: resolve or consciously accept each before proceeding.
4. Summarize the review outcome alongside the change.
