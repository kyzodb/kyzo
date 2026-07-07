---
paths:
  - "kyzo-core/src/data/value/**/*.rs"
  - "kyzo-core/src/data/functions.rs"
  - "kyzo-core/src/data/relation.rs"
  - "kyzo-core/src/data/bitemporal.rs"
  - "kyzo-core/src/lib.rs"
---

# Value Plane Authority

The value plane has separate FORMS with separate authority. Every site chooses one deliberately.

## The three forms

**Canonical bytes** (`data/value/canonical.rs`, with `tag.rs`/`number.rs`/`row.rs`):
- durable storage form, scan-key form, cross-domain identity form
- the ONE value serialization authority (see `storage-serialization.md`)
- NOT the default execution hot-loop form

**Stamped codes / Rows / ExecRows** (`arena.rs`, `column.rs`, `row.rs`, `exec.rs`):
- the within-epoch execution currency
- lawful only under a proven arena + epoch + visibility domain
- raw `u32` identity only AFTER admission
- never persisted across a seal

**Decoded `DataValue`** (`mod.rs`, `cell.rs`, wide faces):
- the API / function / operator surface
- typed semantic operations
- NOT a storage authority

## The byte-order law (on-disk format)

Bytewise order of `encode_owned(v)` MUST equal `v.cmp(w)` (the structural `DataValue::Ord` mirror).
These are two authorities the code declares identical; they must never diverge (a JSON-object
NUL-leading-key divergence was a real caught bug — a present container element must begin with a byte
that outranks the `STRUCT_SEQ_END=0x01` terminator). Tags are globally ordered, tag-byte-first
(`Null=0x05`, `Bool=0x08`, `Num=0x10`, `Str=0x18`, …). `Num` places ints and floats in one exact
real-value order (13-byte key: `[class][exp][frac72][repr]`, `-0.0`→`+0.0`, one NaN, exact beyond
2^53). It is the on-disk format (canonical v1, `FormatVersion` 5): any change to a RELEASED format is
a migration with round-trip + ordering tests and a FormatVersion decision; within the unreleased
value-plane format, correct it in place (no deployed stores).

## Forbidden

- resolving a raw `Code` outside admitted observer/container authority
- spending a `StampedCode` without verification
- forging `CanonicalBytes`, `Minted`, or a wide `Value` without stamp coherence
- serializing `Rows`, or leaking codes out of an `EncodedKey`
- using `DataValue::Ord` as the query *semantic* comparison (that is `Num::cmp_numeric` /
  Allen's relations / typed refusal — order is storage-structural)
- using durable canonical encoding as the fixpoint hot-loop identity when admitted execution rows
  are available

## Comparison authority

Every comparison site must choose: exact numeric authority (`cmp_numeric`/`eq_numeric`), same-kind
storage order (`DataValue::Ord`, == canonical bytes), canonical byte order, observer-backed
comparison (prefix-first, deref only on tie), or typed refusal.
