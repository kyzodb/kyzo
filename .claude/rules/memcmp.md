---
paths:
  - "kyzo-core/src/data/value/canonical.rs"
  - "kyzo-core/src/data/value/tag.rs"
  - "kyzo-core/src/data/value/number.rs"
  - "kyzo-core/src/data/value/row.rs"
  - "kyzo-core/src/data/bitemporal.rs"
---
# Rule: memory-comparable encoding (ON-DISK FORMAT)

`data/value/canonical.rs` encodes `DataValue`s to bytes so that **bytewise order == semantic value
order**. This is the load-bearing invariant of the whole storage layer, and it is what lets one
ordered key-value store serve relational, graph, vector, and text access paths uniformly. (Story #119
unified the old `memcmp.rs`/`fact_payload.rs` split into this one canonical format; the value plane's
owned `DataValue::Ord` is the in-memory structural mirror, LAW-LOCKED to these bytes.)

- It is the **on-disk format** (canonical format v1, `FormatVersion` 5). Any change to tag bytes,
  number encoding, or field order is a **database migration**, not a refactor: it silently corrupts
  existing data and changes index sort order.
- `query/ra`'s `prefix_join` and every range scan depend on this ordering. Break it and joins return
  wrong rows **with no error**. The canonical bytes and `DataValue::Ord` are two authorities the
  code declares identical — they must never diverge (a JSON-object NUL-key divergence was the exact
  bug the story-#119 review caught).
- Hard constraints:
  - Type tags are globally ordered, **tag byte first**, and mirror the value-kind order exactly
    (`Null=0x05`, `Bool=0x08`, `Num=0x10`, `Str=0x18`, … through the temporal kinds — see
    `tag.rs`), one source of truth for cross-type ordering, enforced by the pairwise order-embedding
    law. Never reorder or reuse a tag. Structural bytes `STRUCT_STRING=0x00`/`STRUCT_SEQ_END=0x01`
    sort below every tag; any container element must begin with a byte that outranks the terminator.
  - `Num` (`number.rs`) places ints and floats in ONE exact real-value order via a 13-byte key
    (`[class][exp][frac72][repr]`): `-0.0` collapses to `+0.0`, one canonical NaN, exact beyond
    2^53. Preserve it exactly.
- "UUIDs are not lexicographically sortable" is a *design property* of this encoding, not a hotfix.

The invariant is executable law: `data/value/canonical.rs` and `storage/tests.rs` hold the round-trip,
order-embedding, and byte-flip-corruption property tests (corpus + generative + hand-derived golden
vectors). `EncodedKey` (`data/value/row.rs`) is the typed written key form — only encoders construct
it; the key layout (relation prefix, then the value columns, then the fixed-width bitemporal tail: two
time slots, valid-instant outer, system-version inner — the resolution algebra is
`data/bitemporal.rs`) lives on that type, not as scattered offsets.

**A change here requires:** the law tests passing (round-trip, order embedding, never-panic on corrupt
bytes), and — for a change to an ALREADY-RELEASED format — a `FormatVersion::CURRENT` bump with
migration discussion (stores and dumps are stamped and refuse mismatched versions). Within the value
plane's own unreleased format, correct the format in place (no deployed stores exist).
