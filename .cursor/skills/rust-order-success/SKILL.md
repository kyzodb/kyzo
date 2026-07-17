---
name: rust-order-success
description: Build the one law's compare/encode authority — identity-and-order-before-bytes, the Tag prefix doctrine, structural vs query-semantic order, and unbounded-as-variant — the only place a kyzo value's binary order is allowed to originate. Fires before deriving or hand-writing Ord/PartialOrd/Hash on a domain type, before adding or changing a value kind's byte encoding, before introducing a second cross-type discriminant, before writing a sentinel value (i64::MAX, -1, empty string) to mean "unbounded" or "none", or before an evaluator borrows a model type's Ord for query-semantic comparison.
---

# Order

The one law made construct-level: every stored value encodes to bytes whose binary order equals its semantic order. This file owns compare and encode; `rust-values-success` owns shape. A value's *fields* are proven there; whether its *bytes* sort the way its meaning does is proven here, and nowhere else — `zone-model` states both halves as one sentence, so no site may satisfy one half without the other.

## Identity-and-Order-Before-Bytes

### Definition

Every value kind defines its equality and its ordering as a stated domain law BEFORE its canonical encoding is written, and the encoding is derived to match that law — never the reverse. The canonical encoding is the ONLY byte authority for the value, everywhere it is stored or compared.

### Required Form

```rust
/// Prices order by numeric value; two prices are equal iff their
/// scaled integer representations are equal. (state the law first)
pub struct Price(u64); // fixed-point ticks — unsigned, so big-endian bytes == numeric order.
                       // Never i64 + to_be_bytes(): two's-complement negatives sort *after*
                       // positives as unsigned bytes, silently breaking the one law.

impl Ord for Price {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.cmp(&other.0) // matches the stated law exactly
    }
}

impl Price {
    pub fn to_bytes(self) -> [u8; 8] {
        self.0.to_be_bytes() // unsigned big-endian: byte order == numeric order
    }
}
```

The doctrine comment stating the law is the artifact a reviewer checks the `Ord` impl and the `to_bytes` impl against. A property test proves both independently satisfy it: `x.cmp(&y) == x.to_bytes().cmp(&y.to_bytes())` for all `x, y`. Signed integers need an explicit order-preserving transform before encoding (e.g. bias by mapping `i64` into `u64` so numeric order matches unsigned byte order) — raw `i64::to_be_bytes` is never that transform.

### Sorting Rules

A type with no domain ordering (a value object with no natural rank, an opaque handle) needs no `Ord` at all — do not derive one out of habit. A type whose order is defined only by delegating to another type's already-proven order (a newtype wrapping an already-ordered scalar) inherits that scalar's law and states so explicitly, rather than re-deriving one.

### Replaced Forms

`#[derive(PartialOrd, Ord)]` on a multi-field struct, accepted without checking that the derived lexicographic field order matches a stated domain law, is order by accident: it compiles, and it silently breaks the day a field is reordered or added. An `f64`/`f32` field anywhere in an ordered or stored type is order that breaks on `NaN`, which has no total order — `zone-model`'s determinism requirement forbids this structurally, not just by convention.

### Construct-Specific Doctrine

A byte format, once released, cannot change without a `FormatVersion` decision plus round-trip and ordering tests (`zone-model`, `zone-store`). "Just add a field" to an already-shipped encoding is exactly the change this doctrine gates — it is never a small, local edit.

Where a type also implements `Hash`, the hash must agree with equality: if `a == b` then `hash(a) == hash(b)`. Prefer not implementing `Hash` on ordered domain values that live only in `BTreeMap`/`BTreeSet` keyspaces — `Hash` is not required for the one law, and a derived `Hash` that drifts from `Eq` is a silent correctness bug. Never use `Hash` as a substitute for proving byte order.

### Allowed Patterns

- a one-line doctrine comment stating the domain order law, directly above the type, before any `Ord` or encoding code
- a hand-written `Ord`/`PartialOrd` that implements exactly the stated law
- `#[derive(Ord, PartialOrd)]` only when field declaration order already IS the stated law, stated as such
- fixed-point/unsigned integer representations for anything compared numerically; no `f32`/`f64` in an ordered encoding
- signed values encoded only after an explicit order-preserving bijection into an unsigned byte form
- a property test asserting `Ord::cmp` and byte-comparison of the encoding agree for arbitrary values
- `Hash` only when needed, and only when proven consistent with `Eq`

### Forbidden

- an `Ord`/`PartialOrd` impl (derived or hand-written) with no adjacent stated order law
- `f32`/`f64` as a field type anywhere in a value ever compared or stored in an ordered structure
- raw `i64`/`i32::to_be_bytes` (or equivalent) used as a canonical encoding without an order-preserving transform
- changing a released encoding's byte layout without a `FormatVersion` bump and round-trip + ordering tests
- an encoding whose byte order was never checked against the type's `Ord` impl by a property test
- a `Hash` impl inconsistent with `Eq`, or `Hash` used as if it proved semantic order

### Halt Rule

Halt when a type needs an order but no domain law can be stated for it, or when the encoding cannot be derived to match a stated law without breaking a released format. Report the type and the law: either the order is undecided or the format-version boundary is being crossed silently, and the table is not finished.

## Tag and the Prefix Doctrine

### Definition

One `Tag` type is the sole cross-type discriminant and the sole authority for cross-type order: when values of different kinds are compared or stored in the same ordered space, `Tag` is the leading byte(s) of every encoding, and no second discriminant scheme — a second enum, a type-name string, a locally invented magic number — ever competes with it.

### Required Form

```rust
#[repr(u8)]
pub enum Tag {
    Int = 0,
    Str = 1,
    Bytes = 2,
    // members ordered exactly as the cross-type comparison law states
}

pub fn encode(value: &Value) -> Vec<u8> {
    let mut out = vec![value.tag() as u8]; // Tag is byte 0, always, everywhere
    out.extend(value.encode_payload());
    out
}
```

### Sorting Rules

A within-kind comparison (two `Price`s) never needs `Tag` — it's the identity-and-order-before-bytes doctrine alone. `Tag` is needed exactly where two *different* value kinds must sort against each other in the same keyspace.

### Replaced Forms

A second enum invented to discriminate a subset of kinds "just for this one comparison" is a second order authority the moment it exists. `zone-model` names this exactly: `Tag` is the only type-discriminant and cross-type order authority.

### Construct-Specific Doctrine

`Tag`'s member order on the wire IS the cross-type comparison law; reordering or renumbering its variants is a released-format change and falls under the encoding-change doctrine above.

### Allowed Patterns

- `Tag` as the leading byte(s) of every multi-kind encoding
- `Tag`'s variant order stated as the authoritative cross-type comparison law
- comparison logic that reads `Tag` first, then delegates to the within-kind `Ord` for same-tag values

### Forbidden

- a second discriminant type (enum, string tag, magic number) used to order or distinguish value kinds anywhere `Tag` also applies
- reordering or renumbering `Tag`'s variants without a `FormatVersion` decision
- comparing two differently-tagged encodings by any means other than the `Tag`-first, then-within-kind rule

### Halt Rule

Halt when a new value kind needs to sort against existing kinds and no `Tag` member represents it. Report the kind: `Tag` is missing a member, and the table is not finished.

## Structural vs Query-Semantic Order

### Definition

Two order relations exist over the same values and must never be conflated at any site: STRUCTURAL order (storage and identity — what this file's other constructs prove) and query-semantic order (what a query's ordering operator means during evaluation). `zone-model` states the boundary; evaluation, not the model, owns the second.

### Required Form

```rust
// model crate: structural only
impl Ord for RowKey {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.cmp(&other.0) // storage/identity order — never imported by exec for query semantics
    }
}

// exec crate: query-semantic, defined separately, never delegating to RowKey::cmp
// for anything but genuine identity/storage questions
pub fn evaluate_order_by(rows: &[Row], spec: &OrderBySpec) -> Vec<Row> {
    // comparison logic here is the evaluator's own, keyed to `spec`,
    // and is not assumed equal to any model type's Ord
}
```

### Sorting Rules

If a query-semantic comparison happens to coincide with a structural `Ord` for one type today, that is a coincidence to verify by test, never a delegation to assume by default — the two are allowed to diverge (case-insensitive text ordering, null-ordering policy, locale-aware collation), and the model's `Ord` may never be asked to carry that.

### Replaced Forms

Reaching into the model crate's `Ord` impl from an evaluator operator "because it already sorts them" is the conflation `zone-model` forbids by name. A single `Ord` impl doing double duty for both storage and a configurable query ordering is one structure asked to prove two different laws.

### Allowed Patterns

- structural `Ord`/encoding constructs used only for storage, identity, and keyspace placement
- a distinct, separately defined comparison in the evaluation layer for query semantics, even when it happens to match structural order in a given case
- an explicit test asserting the two coincide, wherever code load-bearingly depends on that coincidence

### Forbidden

- an evaluation operator that imports a model type's `Ord`/`PartialOrd` to implement query-semantic ordering
- one `Ord` impl documented or used as satisfying both structural and query-semantic order
- assuming coincidence between the two without a test that pins it

### Halt Rule

Halt when a query-semantic ordering has no evaluation-layer definition of its own and the only candidate is a model type's structural `Ord`. Report the operator and the type: the query-semantic law is unmodeled here, and the table is not finished.

## Unbounded as a Distinct Variant

### Definition

An unbounded endpoint (no start, no end, "forever") is its own typed variant, byte-distinct and round-trip-distinct from every finite instant — never a sentinel finite value (`i64::MAX`, `-1`, an empty string) pressed into meaning "unbounded."

### Required Form

```rust
pub enum Endpoint {
    At(Timestamp),
    Unbounded,
}
```

`Endpoint::Unbounded` encodes to bytes that sort correctly relative to every `At(Timestamp)` per the stated order law, and no finite `Timestamp` value is reserved to mean the same thing.

### Sorting Rules

If a domain axis has no unbounded case, it needs no such variant — do not add one speculatively. The moment "no bound" needs to be representable, it is this variant, never a magic finite value chosen because it's unlikely to occur.

### Replaced Forms

`Timestamp::MAX` used to mean "no end date" is a sentinel: it is a legal, constructible finite value that also silently means something else, and nothing stops a real timestamp from colliding with it.

### Construct-Specific Doctrine

No sentinel value of this kind ever leaks through a public API — a caller receiving `Endpoint` cannot mistake `Unbounded` for a very large but finite date, because the type makes the two uninhabitable as the same value.

### Allowed Patterns

- an `enum` with a finite variant and an explicit `Unbounded`/`Open`/`Forever`-named variant
- an encoding for the unbounded variant proven distinct and correctly ordered by the same property test as the rest of the type

### Forbidden

- a finite sentinel value (`MAX`, `-1`, empty string, epoch zero) used to mean an unbounded or absent endpoint
- a sentinel of this kind crossing any public API boundary

### Halt Rule

Halt when an axis needs an unbounded case and no variant exists for it, or when a sentinel is already in use and cannot be replaced without a `FormatVersion` decision. Report the axis and the sentinel: the shape is unbounded-unmodeled, and the table is not finished.
