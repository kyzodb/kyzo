---
name: rust-order-failure
description: Fires when a kyzo value's binary order is about to be produced or compared wrong instead of via the matching construct in rust-order-success — a derived or hand-written Ord/Hash with no stated order law, an f32/f64 field in an ordered/stored type, a second discriminant competing with Tag, an evaluator importing a model type's Ord for query semantics, a sentinel value standing for "unbounded" or "none", or a released encoding changed with no FormatVersion decision.
---

# Order — failure patterns

Ways a value's binary order gets produced or compared wrong instead of proven by the constructs in `rust-order-success`.

## Silent derive

`#[derive(Ord, PartialOrd)]` compiles for any struct with orderable fields, whether or not the derived lexicographic-by-declaration-order comparison matches any stated domain law. It silently breaks the day a field is reordered or a field is added above the one that mattered.

```rust
#[derive(PartialOrd, Ord)]
pub struct Candle {
    close: Price,
    open: Price,
    high: Price, // reordering these fields silently changes Candle's Ord: state the domain order law first, then derive or hand-write to match it
}
```

## Float in an ordered or stored type

`f32`/`f64` has no total order (`NaN`), so any type containing one cannot be soundly compared, hashed, or stored in an ordered structure.

```rust
pub struct Quote {
    mid: f64, // no total order, breaks determinism (zone-exec, zone-model): fixed-point or integer representation instead
}
```

## Signed bytes without an order-preserving transform

Raw `i64::to_be_bytes` (and friends) do **not** yield byte order equal to numeric order: two's-complement negatives sort after positives when compared as unsigned bytes. That is a silent one-law defect.

```rust
fn to_bytes(price: i64) -> [u8; 8] {
    price.to_be_bytes() // -1 sorts after 0 as bytes: use unsigned ticks, or bias i64→u64 before encoding
}
```

## Hash inconsistent with Eq

A `Hash` impl (derived or hand-written) that can disagree with `Eq` makes equal values diverge in hash maps and silently corrupts any structure that depends on the pair.

```rust
// Eq compares scaled ticks; Hash hashes a display string — equal Prices can hash differently.
// Prove hash(a) == hash(b) whenever a == b, or do not implement Hash at all on BTree-keyed domain values.
```

## Second discriminant beside `Tag`

A locally invented enum, string tag, or magic number used to distinguish or order value kinds anywhere `Tag` also applies is a second order authority the moment it's compared against anything `Tag`-tagged.

```rust
pub enum LocalKind { IntLike, StrLike } // competes with Tag as a discriminant: route through Tag, the one cross-type authority
```

## Structural/query-semantic conflation

An evaluation operator importing a model type's `Ord` to implement query-ordering semantics assumes the two orders coincide without proof, and breaks the moment they're asked to diverge (case-insensitive sort, null-ordering policy).

```rust
rows.sort_by(|a, b| a.row_key.cmp(&b.row_key)); // this IS structural order — if the query asked for a different ORDER BY semantics, this silently answers the wrong question
```

## Sentinel value for "unbounded" or "none"

A finite value reused to mean "no bound" or "missing" is constructible as an ordinary value too, so nothing stops a real occurrence from colliding with the sentinel's meaning.

```rust
const NO_END: i64 = i64::MAX; // a real timestamp could equal this: Endpoint::Unbounded as its own variant instead
```

## Unversioned format change

Adding, removing, or reordering fields in an already-released byte encoding with no `FormatVersion` decision silently breaks every existing on-disk byte sequence's meaning.

```rust
// v1 encoding shipped; someone adds a field to the same byte layout with no FormatVersion bump:
// old bytes now decode with the new field reading garbage from what used to be the next value's prefix
```

## Untested order/encoding agreement

An `Ord` impl and a `to_bytes`/encode impl written independently, with no property test asserting they agree, are two laws that happen to match today and are free to drift apart on the next edit to either.

```rust
// Price::cmp and Price::to_bytes both exist, but no test asserts
// x.cmp(&y) == x.to_bytes().cmp(&y.to_bytes()) for arbitrary x, y — add the property test
```

## Standing ban: `unsafe`

`#![forbid(unsafe_code)]` applies repo-wide across every `rust-*` group. `unsafe` is never a legal shortcut for any construct here. If an encoding or comparison seems to need `unsafe` to exist, the construct is wrong, not the ban.
