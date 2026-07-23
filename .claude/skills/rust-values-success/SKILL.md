---
name: rust-values-success
description: Build the five proven shapes for a kyzo domain fact — newtype scalar, value object, concept struct, proven collection, sum type — the only representations for data inside the engine. Fires before writing a bare primitive field, a pub field on a would-be newtype, a struct composing raw primitives, a Vec or HashMap field, an Option field standing for a meaningful absence, a String-typed kind/status field, or a match/if-let chain selecting behavior by an unproven discriminant.
---

# Values

The data-shape constructs: what a kyzo domain fact *is*, proven by construction so an invalid fact cannot exist as a value. One axis per pattern — scalar (one value), value object (a few values, no identity), concept struct (a full thing), proven collection (a sequence or map that is itself a domain fact), sum type (a closed choice among structures). Byte identity and comparison order for these values is `rust-order`'s doctrine, not this file's: a value's *shape* is proven here; whether its *bytes* sort the way its meaning does is proven there. Never restate order doctrine here, and never let this file's constructs skip it.

## Branded Proof (applies to every construct below)

A value produced by a fallible constructor — a smart constructor, a `TryFrom`, a boundary decode — carries its proof forward as the type itself. `zone-model` states it as law: a value lifted at a boundary carries its proof forward as a branded fact, and no downstream site re-checks what a constructor already proved. A function receiving a `Price` never re-validates that it's positive; the type is the certificate. A call site that re-checks an already-typed value is either dead code (delete it) or evidence the type doesn't actually prove what its name claims (fix the type, don't add the check).

## Newtype Scalar

### Definition

A `struct X(T)` (or a struct with one private field) over one primitive or one closed value space, whose only public constructor validates the domain's constraint at the boundary of construction. The atomic domain value: it references no other domain type.

### Required Form

```rust
pub struct Price(Decimal);

impl Price {
    pub fn new(value: Decimal) -> Result<Self, PriceError> {
        if value > Decimal::ZERO {
            Ok(Self(value))
        } else {
            Err(PriceError::NotPositive(value))
        }
    }

    /// Only at sites that already hold the proof (post-decode, or a
    /// derivation whose inputs already constrain the result).
    pub(crate) fn new_unchecked(value: Decimal) -> Self {
        Self(value)
    }

    pub fn get(&self) -> Decimal {
        self.0
    }
}

pub enum Side {
    Buy,
    Sell,
}
```

`Price`'s field is private; the crate has exactly one public way to produce a `Price`, and that way cannot be bypassed by writing the tuple-struct literal directly, because the tuple struct's field isn't public. `new_unchecked` is `pub(crate)` for sites that already hold the proof — never a public escape hatch. A closed vocabulary wraps a plain `enum`, never a `String` or a bag of `&'static str` constants.

### Sorting Rules

One axis, every member the same kind of thing: a scalar. A member needing a field or behavior a sibling lacks: two axes, a sum type. A value composed of other declared values: a value object or concept struct.

### Replaced Forms

A bare `f64`/`String`/`i64` field standing for a domain quantity holds its meaning in a variable name no downstream reader receives. A `String` used as a closed vocabulary is unnamed and unproven — nothing stops `"boyu"` from typo-ing past `"buy"`. A `pub` field on an otherwise well-named wrapper is a lock with the key taped to the door: any module can construct an invalid instance directly.

### Construct-Specific Doctrine

The private field is not optional politeness — it is the entire enforcement mechanism. A `struct X(pub T)`, or a struct with a `pub` field, proves nothing: anyone can write `X(bad_value)`. An unchecked constructor (`new_unchecked`, a validating-nothing `From<T>`) is legal only as `pub(crate)` or narrower, reserved for sites that already hold a proof — decoding bytes this crate already validated on write, for instance.

### Allowed Patterns

- `struct X(T)` with a private field and one fallible public constructor validating the domain bound
- `struct X(E)` over a plain `enum` closed value space
- a `pub(crate)` or private unchecked constructor at a site that already holds the proof (post-decode, post-validation)
- a derivation method (`&self -> DeclaredType`) selecting by data lookup on a closed space, never a branch

### Forbidden

- a `pub` field on a newtype (defeats the only enforcement point)
- a bare primitive used as a domain value with no wrapping type
- `String`/`&str` used as a closed vocabulary instead of an `enum`
- a public unchecked constructor reachable from outside the crate
- a `match`/`if`/`else if` ladder inside a scalar's derivation where a data lookup suffices

### Halt Rule

Halt when a member of the value space needs a field or behavior a sibling lacks, or when a value has neither a statable constraint nor a statable reason the open range is the domain fact. Report the type and the member: the meaning is a sum type or is not yet understood, and the table is not finished.

## Value Object

### Definition

A small `struct` composing scalars into a value with no identity, equal by value (`#[derive(PartialEq)]` alone, no `id` field): a measurement, a spread, a duration. The composition layer between the newtype scalar and the concept struct.

### Required Form

```rust
#[derive(Clone, PartialEq)]
pub struct Spread {
    best_bid: Price,
    width: SpreadWidth,
}

impl Spread {
    pub fn new(best_bid: Price, width: SpreadWidth) -> Self {
        Self { best_bid, width }
    }

    pub fn best_ask(&self) -> Price {
        // bid is positive and width is non-negative by construction —
        // the sum stays inside Price's invariant; no Result to unwrap.
        Price::new_unchecked(self.best_bid.get() + self.width.get())
    }
}
```

### Sorting Rules

A single value is a newtype scalar. A full domain thing or fact, anything with identity or a discriminant, is a concept struct. A value object never holds a handle or client, never pins a discriminant, and two value objects with equal fields are the same value.

### Replaced Forms

A tuple or a raw `(Decimal, Decimal)` pair carries the parts without the name or the proof. A struct whose fields can independently vary into an invalid combination, guarded only by a runtime check called after construction, is the relation left unmodeled — reparameterize instead of guarding.

### Construct-Specific Doctrine

A relation no single field constrains is part of the composite's construction, never a check after it: reparameterize so the relation collapses into one constrained field plus a derivation. `Spread` holds a base and a non-negative `width`, and derives `best_ask`, so an inverted spread has no representation at all — not "a spread that fails a check," but a value that cannot be named.

### Allowed Patterns

- a plain `struct` with private fields, all scalars or value objects, one constructor
- a cross-field relation reparameterized into one constrained field plus a derivation method
- derivation methods (`&self -> DeclaredType`) implying the value's implied facts

### Forbidden

- a bare primitive field
- an `Option<T>` field
- an identity or discriminant field
- a handle, client, or resource as a field
- a standalone validation function called after construction to police a relation between fields
- a stored field that a derivation could compute from the others

### Halt Rule

Halt when the value needs identity, a discriminant, or a field that is itself a full domain concept, or when a relation resists reparameterization. Report the type and the field or relation: the meaning is a concept struct or an unfactored concept, and the table is not finished.

## Concept Struct

### Definition

A `struct` composing declared types into one full domain thing or fact. The product type whose sum-type sibling is the enum: the struct is the concept, each field a relation to another declared type.

### Required Form

```rust
pub struct Fill {
    order_id: OrderId,
    account_id: AccountId,
    fill_price: Price,
    filled_quantity: Quantity,
}

impl Fill {
    pub fn exposure(&self) -> Exposure {
        Exposure::from_product(self.fill_price, self.filled_quantity)
    }
}

pub struct OpenPosition {
    fill: Fill,
    prior_exposure: Exposure,
}

impl OpenPosition {
    pub fn exposure(&self) -> Exposure {
        Exposure::saturating_add(self.prior_exposure, self.fill.exposure())
    }
}

pub enum AccountPosition {
    Open(OpenPosition),
    Flat, // absence of a position is its own variant — no Option<OpenPosition>
}
```

Every field is a declared type: never a bare primitive, never `Option<T>` standing for absence with no named meaning, never a value a derivation could compute. A struct whose discriminant is pinned by an enclosing enum's variant is a sum-type variant (Sum Type, below), not a standalone concept. `Exposure::from_product` / `saturating_add` are infallible constructions over already-proven scalars — not `Result`-returning smart constructors papered over with `.unwrap()`.

### Sorting Rules

A small identity-less composition of scalars, equal by value, is a value object. A single value is a newtype scalar. A choice among concept structs over one axis is a sum type; a concept struct that is one arm of that choice is that sum type's variant.

### Replaced Forms

A struct with public fields and no constructor carries the shape with no proof it was ever validated as a whole. A validator function called after `Self { .. }` construction is a check performed after the fact; the relation it polices reparameterizes into the structure instead. A trait with one impl, created only to share fields between two otherwise-unrelated structs, is a second structure for one meaning; the shared field is already shared as the value type both compose.

### Construct-Specific Doctrine

**Construction discipline.** A composite constructs whole, in one expression: constituents are proven by their own fallible constructors called inside that expression, never pre-built in separate `let` bindings staged before the composite literal. A `TryFrom` impl on the composite is the whole lift from foreign data; a hand-written field-by-field copy where `TryFrom` composes the same fields is a mapper in miniature.

**Absence.** "May be missing" is never a field. Absence that means something is a sum-type variant named for what absence means, or a separate struct when absence changes the state's shape entirely. When lifting from foreign data, an omitted field resolves to a default that states what omission means, or a variant when omission means a different fact; a bare `None` never crosses into the domain unnamed.

`Flat` is the account with no position, not `Option<OpenPosition>`: absence changed the state's shape, so absence is its own variant (see `AccountPosition` above).

### Allowed Patterns

- a `struct` with private fields, every field a declared type: a scalar, a value object, a collection, a concept struct, or a sum-type variant
- a `TryFrom`/`From` impl as the whole lift from a foreign or contract shape
- a defaulted field whose default states what omission means
- derivation methods implying the struct's facts
- a discriminant field only as an enclosing enum's variant tag, never as a standalone field on a freestanding struct

### Forbidden

- a `pub` field with no constructor enforcing whole-struct invariants
- an `Option<T>` field standing for unnamed absence
- a stored field a derivation could compute from the others
- a free function that validates a constructed value after the fact
- a marker trait or shared base implemented only to reuse fields across unrelated structs
- a constituent built in a separate statement before the composite literal that holds it

### Halt Rule

Halt when a field has no declared type to hold it, when a cross-field relation will not reparameterize into a constraint plus a derivation, or when an absence has no statable meaning. Report the type and the field or relation: the meaning is not yet modeled, and the table is not finished.

## Proven Collection

### Definition

A `struct` wrapping `Vec<T>`, `BTreeMap<K, V>`, or `BTreeSet<T>` behind a private field, for a sequence or namespace that is itself a domain fact with its own name, constraint, or derivation. A sequence with no meaning of its own is a `Vec<T>` field on a struct, not a named collection. `HashMap`/`HashSet` are forbidden as the backing store for any proven collection — their iteration order is not the semantic order the one law requires (`rust-order-success`), so a collection built on them cannot be the deterministic, byte-ordered fact `zone-exec` and `zone-store` require.

### Required Form

```rust
pub struct Fills {
    rows: Vec<Fill>, // private; invariant proven at construction
}

impl Fills {
    pub fn new(mut rows: Vec<Fill>) -> Result<Self, FillsError> {
        if rows.is_empty() {
            return Err(FillsError::Empty);
        }
        rows.sort_by(|a, b| a.order_id.cmp(&b.order_id));
        rows.dedup_by(|a, b| a.order_id == b.order_id);
        Ok(Self { rows })
    }

    pub fn as_slice(&self) -> &[Fill] {
        &self.rows
    }
}

pub struct PriceBook {
    prices: BTreeMap<ProductId, Price>, // key order IS the semantic order
}

impl PriceBook {
    pub fn price_of(&self, product: &ProductId) -> Option<&Price> {
        self.prices.get(product)
    }
}
```

The collection constructs whole, from one produced `Vec`/`BTreeMap`, never built by looping `push`/`insert` calls against a field already exposed as `pub`. When the collection's meaning includes order or uniqueness, the constructor proves sort and dedup — not merely non-empty.

### Sorting Rules

An element that is a bare primitive is an undeclared newtype scalar: build the scalar first. A sequence with no name, constraint, or derivation of its own is a plain `Vec<T>`/`BTreeMap<K, V>` field on a value object or concept struct — still private, still declared-type elements, just not its own named type. A fact the sequence implies as a whole is a derivation method on the named collection.

### Replaced Forms

A `pub rows: Vec<T>` field is an unconstrained, externally-mutable container where a proven sequence belongs. A loop appending into a `Vec` across several statements, with the invariant checked afterward or not at all, is construction performed as procedure; the iterator chain producing the whole `Vec` in one expression is the entire build.

### Construct-Specific Doctrine

A namespace keyed by a domain value, where key-uniqueness is the domain fact, is a keyed collection: `BTreeMap<K, V>` behind a private field, `K` a declared scalar. Pair it with a query struct holding the collection and a key, whose `answer()` method returns a found-or-missing sum type — never a raw `.get(&key)` returning `Option<&V>` read directly by a consumer, because `Option` at the call site is exactly the unnamed-absence failure this doctrine forbids in domain-facing code.

```rust
pub struct PriceQuery<'a> {
    book: &'a PriceBook,
    product: ProductId,
}

pub enum PriceAnswer {
    Found(Price),
    Missing(ProductId),
}

impl PriceQuery<'_> {
    pub fn answer(&self) -> PriceAnswer {
        match self.book.price_of(&self.product) {
            Some(price) => PriceAnswer::Found(*price),
            None => PriceAnswer::Missing(self.product),
        }
    }
}
```

### Allowed Patterns

- a private `Vec<T>`/`BTreeMap<K, V>`/`BTreeSet<T>` field on a value object or concept struct, `T`/`K`/`V` declared types
- `struct Xs { rows: Vec<T> }` (or `BTreeMap`/`BTreeSet`) with a fallible constructor when the sequence carries its own bound
- sort and/or dedup performed inside that constructor when order or uniqueness is part of the collection's meaning
- a keyed collection (`BTreeMap`) exposing lookup only through an accessor or query struct, never by reaching into the private map from outside
- a keyed collection paired with a query struct returning a found-or-missing sum type
- derivation methods on the named collection returning declared types
- the collection constructed whole from one iterator chain or one produced container

### Forbidden

- a `pub` `Vec`/`BTreeMap`/`BTreeSet` field on a domain struct
- `HashMap`/`HashSet` as the backing store for any proven collection or domain-facing keyed lookup
- a collection element typed as a bare primitive
- a loop pushing/inserting into a collection across multiple statements where a constructor should build it whole
- `.get(&key)` returning `Option<&V>` read directly by domain-facing code instead of through a query struct's found-or-missing answer

### Halt Rule

Halt when the element is not a declared type, or when a cross-element rule cannot be expressed as a constructor invariant or a query struct's `answer()`. Report the type and the rule: the element or the question is not yet modeled, and the table is not finished.

## Sum Type

### Definition

A closed `enum` of two or more variants over one domain axis, each variant carrying exactly the fields that differ for that case. The structural form of a choice: a decision with consequences is a sum type of outcome variants, never a `bool`; identity-carrying data crossing a boundary constructs through a discriminated lift (`rust-adapters-success`), never through this enum matched directly on raw bytes.

### Required Form

```rust
pub enum OrderOutcome {
    Filled { fill_price: Price, filled_quantity: Quantity },
    Rejected { reason: RejectionReason },
}

impl OrderOutcome {
    pub fn headline(&self) -> Headline {
        match self {
            OrderOutcome::Filled { .. } => Headline::new("order filled"),
            OrderOutcome::Rejected { .. } => Headline::new("order rejected"),
        }
    }
}
```

A fact that differs by variant is a same-named method matched once, at the type's own `impl` block — the one legal `match` on this axis. A consumer calls `outcome.headline()`; it never matches `OrderOutcome` itself to re-derive what the type already computed.

Variants with identical payloads still differ in nothing but identity, so identity is a field or it is nowhere:

```rust
pub enum HaltEvent {
    Halted { at: Timestamp },
    Resumed { at: Timestamp },
}
```

A yes-or-no decision with consequences is a two-variant enum, never `bool`, because each outcome carries its own facts:

```rust
pub enum Review {
    Approved { terms: ApprovalTerms },
    Refused { reason: RefusalReason },
}
```

### Sorting Rules

A uniform one-axis vocabulary, every member the same kind of thing, is a newtype scalar over a plain `enum`; the moment a member needs a field or behavior a sibling lacks, it is this construct. Foreign data expected sometimes to fail constructs through an ordered lift (`rust-adapters-success`), never by matching this enum against raw input. A single variant considered alone, with no sibling, is a concept struct.

### Replaced Forms

A `match`/`if`/`else if`/`is_*()` ladder written at a call site, re-deriving what differs by variant, is selection re-implemented after construction already selected; what differs by variant is that variant's own method, matched exactly once at the `impl`. A `bool` decision fuses both outcomes into one bit and drops both payloads. A `String`/`&str` discriminant, or an integer tag matched by hand, admits values the axis never declared and proves nothing at compile time.

### Construct-Specific Doctrine

The one legal `match` on a sum type's own axis lives inside that type's `impl` block, as the body of a method every variant must handle — the compiler's exhaustiveness check is the enforcement, not a runtime default arm. A `match` on the same axis appearing again at a call site is that method escaped from its owner: inline it back as a method and call the method.

### Allowed Patterns

- a closed `enum` with two or more variants, each carrying exactly its own differing fields
- a `match` on the enum's own variants, exhaustive (no wildcard `_` arm swallowing a case), living inside that type's `impl` block as a same-named method every variant answers
- a two-variant enum for a yes/no decision whose outcomes carry different facts
- `#[non_exhaustive]` only when the axis is genuinely open to a future crate version, never as cover for an unmodeled variant today

### Forbidden

- a `match`/`if let`/`is_*()` chain over the same variants repeated at more than one call site instead of one method on the type
- `bool` returned as, or used to encode, a domain decision with distinct consequences per outcome
- a `String`/`&str`/raw integer discriminant matched by hand instead of an `enum`
- a wildcard `_` arm on a `match` over a closed sum type's own axis, hiding an unhandled variant
- constructing a variant from untrusted raw input without going through the ordered lift in `rust-adapters-success`

### Halt Rule

Halt when a variant cannot be told apart from its siblings by structure alone, when a member sits outside the axis, or when the data that must construct the sum type carries no reliable discriminant. Report the type and the variant or arrival: the axis is mis-factored or the arrival belongs to `rust-adapters-success`'s ordered lift, and the table is not finished.
