---
paths:
  - "crates/kyzo-core/src/store/**/*.rs"
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
- A cursor's validity is tied by type to the snapshot or transaction it was
  opened against; a cursor escaping its snapshot is a borrow-check error, not a
  runtime guard.
- Every acquired resource — open cursor, lock, scratch store, live transaction
  — releases through `Drop` bound to its scope; an unfinished protocol whose
  silent drop would corrupt an invariant is a drop-bomb (a safe terminal
  default or a named panic), never a forgettable call site.
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
- The store owns the portable dump/load FORMAT only; replication is
  NATS/JetStream replay of the ordered write log against engine determinism —
  provably-equal replicas, never a replicator written in this zone.
- Commit door is SweepDoor: IntentOrdinal orders contenders (may gap);
  CommitOrdinal is dense history assigned only at the durable event.
- WriteAuthority signs; IncarnationId separates write-session nonce space;
  NonceLease is pipelined (durable before ciphertext).
- Sealed artifacts use one CanonicalTranscript; AdmissionCertificate /
  AcceptedReplica verify — never reshape origin-cut meaning.
- ObjectDurabilityClass is a product under dominance; Repair/Downgrade typed.
- Claim tags: Unconstructible / Refused / Unexposed — never soft folklore.

## Forbidden

- Interpreting values beyond their order (the store consumes canonical bytes;
  meaning lives in the model).
- A second storage backend without an operator ruling plus conformance-kit
  passage — never as a convenience.
- C/C++ anywhere in the dependency tree.
- Nondeterministic iteration or timing-dependent behavior on any path that
  affects committed state.
- Minting history at admission; encrypting under a volatile NonceLease;
  in-place reinterpretation of a certified replica record.
