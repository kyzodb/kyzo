---
paths:
  - "kyzo-core/src/store/**/*.rs"
---

# Zone: Store — fact persistence

The one substrate: ordered keys, transactions, time in the key, crash safety.

## Required

- Everything transactional: no write path outside a transaction; commits are
  consuming (the type is spent); conflicts are typed values.
- Keys are memcomparable-ordered; time lives in the key per the bitemporal law
  (one fact key answers a two-axis question).
- Species invariants held by TYPES: a reader cannot write; never move an
  invariant down the enforcement ladder to runtime checks or convention.
- Any input from disk decodes to typed refusal on corruption — no panic on any
  byte sequence.
- The skip-scan walk has ONE implementation, generic over its driver.
- The state root is deterministic over the ordered keyspace.
- Commit survives a process crash; durable commit survives a power cut —
  both proven, never assumed.
- The commit clock is a monotone watermark, and stamp minting takes the open
  snapshot as an argument so mint-before-snapshot is unrepresentable.
- Any non-value serialization boundary (catalog metadata, manifests) is a
  single ruled door: config only, never a value, its own FormatVersion,
  typed corruption behavior. "Metadata only" is never an argument for a
  second value authority.

## Forbidden

- Interpreting values beyond their order (the store consumes canonical bytes;
  meaning lives in the model).
- A second storage backend without an operator ruling plus conformance-kit
  passage — never as a convenience.
- C/C++ anywhere in the dependency tree.
- Nondeterministic iteration or timing-dependent behavior on any path that
  affects committed state.
