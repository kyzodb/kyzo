---
name: rust-values-failure
description: Fires when kyzo domain data is about to be shaped wrong instead of as the matching construct in rust-values-success — a bare primitive field, a pub field on a would-be newtype, an unproven Vec/HashMap, an Option<T> standing for meaningful absence, a String-typed kind/status field, a match/if-let ladder re-deriving a sum type's own fact, a HashMap backing a domain collection, a bool encoding a domain decision, or a field the ontology never declared.
---

# Values — failure patterns

Ways domain data gets shaped wrong instead of built as a newtype scalar, value object, concept struct, proven collection, or sum type (`rust-values-success`).

## Bare primitive

A primitive field names a domain quantity but keeps none of its constraint; the constraint then scatters into checks instead of living in the type.

```rust
pub struct Account {
    pub email: String, // "email" is a claim the type doesn't keep: wrap it as a newtype scalar carrying the constraint
}
```

## `pub` field on a newtype

A `pub` field on a single-field wrapper defeats the only enforcement point a newtype has: any module can construct `Price(Decimal::NEG_ONE)` directly, bypassing the constructor entirely.

```rust
pub struct Price(pub Decimal); // any caller can build an invalid Price directly: make the field private, add a fallible new()
```

## Free-floating vocabulary

A closed value-space with no scalar or enum that owns it: a raw `String`/`&str` used as a field. Nothing proves membership at compile time.

```rust
status: String, // "open" / "filled" / "canceled" typed by hand: a plain enum, wrapped as a newtype scalar if it needs a constraint beyond membership
```

## Mutable, unproven bag

A `pub Vec`/`HashMap`/`HashSet` field is unproven and externally mutable: nothing guarantees its contents are valid, and any caller can push an invalid element or mutate it after construction.

```rust
pub struct Book {
    pub bids: Vec<Level>, // not a constructed Proven Collection: private field, fallible constructor, contents proven whole
}
```

## `HashMap`/`HashSet` where order is the domain fact

`HashMap` and `HashSet` iterate in an order that is not the semantic order the one law requires. Any domain-facing collection, or anything that touches determinism (`zone-exec`, `zone-store`), needs `BTreeMap`/`BTreeSet` instead.

```rust
prices: HashMap<ProductId, Price>, // iteration order isn't semantic order: BTreeMap<ProductId, Price>, per rust-order-success
```

## `Option<T>` absence

A `T | None`-shaped field models absence as a slot modifier instead of a state. "Missing" is a fact about which world the value is in; fusing it into one field forces every reader to branch and names neither state.

```rust
pub struct Order {
    pub settled_at: Option<DateTime<Utc>>, // two states crammed into one field: a Settled variant carrying the timestamp, an Unsettled variant carrying none
}
```

## Sum type re-matched at call sites

A `match`/`if let`/`is_*()` ladder over a sum type's variants, written again at a consumer instead of once on the type, restates selection construction already performed.

```rust
let headline = match outcome {
    OrderOutcome::Filled { .. } => "order filled",
    OrderOutcome::Rejected { .. } => "order rejected",
}; // re-derives what the type should answer: outcome.headline(), a method on OrderOutcome's own impl
```

## `bool` as a domain decision

A `bool` fuses two outcomes with distinct consequences into one bit and drops both payloads.

```rust
fn is_approved(&self) -> bool { .. } // Approved carries terms, Refused carries a reason — a two-variant enum, not a flag
```

## Validator-after-construction

A free function or method that checks a relation between fields after `Self { .. }` has already been built polices a state the structure let exist in the first place.

```rust
fn validate(order: &Order) -> Result<(), OrderError> {
    if order.bid > order.ask { return Err(OrderError::Inverted); } // reparameterize: base + non-negative offset, derive the other side
    Ok(())
}
```

## Redundant re-check of a branded value

A function re-validating a value whose type already proves the invariant is dead code at best, evidence the type lies about what it proves at worst.

```rust
fn charge(price: Price) {
    assert!(price.get() > Decimal::ZERO); // Price::new already proved this; either delete the assert or Price doesn't actually enforce its own invariant
}
```

## Field the ontology never declared

Class and catalog disagree. An extra field is unmodeled meaning entering source; a missing field is modeled meaning never rendered.

```rust
pub struct SignalSnapshot {
    spread: Spread,
    urgency: i32, // no row declares this: place the meaning in the ontology, or remove the field
}
```

## Standing ban: `unsafe`

`#![forbid(unsafe_code)]` applies repo-wide across every `rust-*` group. `unsafe` is never a legal shortcut for any construct here. If a value construct seems to need `unsafe` to exist, the construct is wrong, not the ban.
