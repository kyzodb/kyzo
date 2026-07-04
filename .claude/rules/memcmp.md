---
paths:
  - "kyzo-core/src/data/memcmp.rs"
  - "kyzo-core/src/data/tuple.rs"
  - "kyzo-core/src/data/bitemporal.rs"
  - "kyzo-core/src/data/fact_payload.rs"
---
# Rule: memory-comparable key encoding (ON-DISK FORMAT)

`memcmp.rs` encodes `DataValue`s to bytes so that **bytewise order == semantic value order**. This is
the load-bearing invariant of the whole storage layer, and it is what lets one ordered key-value store
serve relational, graph, vector, and text access paths uniformly.

- It is the **on-disk format**. Any change to tag bytes, number encoding, or field order is a
  **database migration**, not a refactor: it silently corrupts existing data and changes index sort order.
- `ra.rs::prefix_join` and every range scan depend on this ordering. Break it and joins return wrong rows
  **with no error**.
- Hard constraints:
  - Type tags are globally ordered (`NULL_TAG=0x01` … `BOT_TAG=0xFF`) and mirror the `DataValue`
    enum's declaration order exactly — one source of truth for cross-type ordering, enforced by the
    pairwise order-embedding law. Never reorder or reuse a tag; never reorder the enum.
  - Numbers are BigEndian. Ints and floats share one sortable space via `EXACT_INT_BOUND` +
    `IS_FLOAT`/sign-flip: preserve it exactly.
- "UUIDs are not lexicographically sortable" is a *design property* of this encoding, not a hotfix.

The invariant is executable law: `storage/tests.rs` holds the round-trip and order-embedding property
tests (corpus + generative + byte-flip corruption). `EncodedKey` (tuple.rs) is the typed written form —
only encoders construct it; the key layout (relation prefix, fixed-width bitemporal tail (two time slots: valid-instant outer, system-version inner — the resolution algebra over them is `data/bitemporal.rs`; the value's tagged-field layout is `data/fact_payload.rs`, FormatVersion 3)) lives on that
type, not as scattered offsets.

**A change here requires:** the law tests passing (round-trip, order embedding, never-panic on corrupt
bytes), and a `FormatVersion::CURRENT` bump with migration discussion — stores and dumps are stamped
and refuse mismatched versions.
