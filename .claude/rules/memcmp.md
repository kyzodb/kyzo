---
paths:
  - "kyzo-core/src/data/memcmp.rs"
  - "kyzo-core/src/data/tuple.rs"
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
  - Type tags are globally ordered (`NULL_TAG=0x01` … `BOT_TAG=0xFF`) and define cross-type ordering:
    never reorder or reuse a tag.
  - Numbers are BigEndian. Ints and floats share one sortable space via `EXACT_INT_BOUND` +
    `IS_FLOAT`/sign-flip: preserve it exactly.
- "UUIDs are not lexicographically sortable" is a *design property* of this encoding, not a hotfix.

**A change here requires:** a round-trip encode/decode + ordering test, and an explicit
format-versioning / migration discussion.
