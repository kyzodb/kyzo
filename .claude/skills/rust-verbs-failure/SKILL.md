---
name: rust-verbs-failure
description: Fires when kyzo behavior is about to be written wrong instead of as the matching construct in rust-verbs-success — a free function computing from a struct's fields from outside the type, a &mut self method mutating nested state in place instead of reassigning it, a sealed/committed bool flag policing a lifecycle the type system should enforce, unwrap/expect/panic! on a reachable input, a match ladder over a sum type's variants repeated at more than one call site, or a stub method body (todo!()/unimplemented!()).
---

# Verbs — failure patterns

Ways behavior gets written wrong instead of built as a derivation, transition, or consuming verb (`rust-verbs-success`).

## Free-floating derivation

A free function or a `helpers`/`utils` module function computing a fact from a struct's public fields is that struct's own derivation, escaped from its owner.

```rust
fn exposure(fill: &Fill) -> Exposure { .. } // belongs on Fill: impl Fill { pub fn exposure(&self) -> Exposure { .. } }
```

## In-place mutation of state

Reaching into a live handle's current state and mutating a nested field, instead of constructing the new state whole and reassigning it, leaves a half-updated value observable mid-call and duplicates the construction logic that should live in one place.

```rust
self.latest.exposure = self.latest.exposure + delta; // mutates a proof's contents: construct a new PositionState and reassign self.latest to it
```

## Bool-flag lifecycle

A `sealed`/`committed`/`ready` flag, checked at the top of methods to police what phase a value is in, is a lifecycle boundary enforced by convention instead of by the type system.

```rust
pub struct Index {
    sealed: bool,
    rows: Vec<Row>,
}

impl Index {
    pub fn search(&self, q: &Query) -> Candidates {
        if !self.sealed { panic!("not sealed"); } // a BuildingIndex/SealedIndex type split makes this call uncallable instead of a runtime panic
        ..
    }
}
```

## Panic on reachable input

`unwrap`/`expect`/`panic!` on a value that can legitimately arrive from outside the crate turns a typed refusal into a crash.

```rust
pub fn book(&mut self, report: VenueFill) {
    let fill = Fill::try_from(report).unwrap(); // report is foreign; try_from can legitimately fail — return Result, or capture-then-lift per rust-adapters-success
}
```

## Repeated match ladder

A `match`/`if let` over a sum type's variants, written again at a second or third call site instead of once as a method on the type, is the same selection logic duplicated and free to drift.

```rust
// call site A:
match outcome { OrderOutcome::Filled { .. } => log_fill(), OrderOutcome::Rejected { .. } => log_reject() }
// call site B, same match repeated: both collapse into outcome.headline() or a single method on OrderOutcome
```

## Stub verb

A method body of `todo!()`, `unimplemented!()`, or bare `()` treats the signature as a placeholder to fill later — but a verb's row is the work itself, not a TODO.

```rust
pub fn settle(&mut self) -> Receipt {
    todo!() // construct the Receipt and re-point the state field to it now
}
```

## Fetch-and-return dressed as a transition

A `&mut self` method that only retrieves and returns a field, doing no construction and no reassignment, is a repository surface wearing a transition's signature.

```rust
pub fn latest(&mut self) -> &PositionState {
    &self.latest // no transition happens here: this is a derivation-shaped read; drop the &mut, or add the actual transition this name implies
}
```

## Standing ban: `unsafe`

`#![forbid(unsafe_code)]` applies repo-wide. `unsafe` is never a legal shortcut for any construct in this group — not to skip a `Result` return on a fallible operation, not to bypass a consuming verb's ownership guarantee via a raw pointer. If a verb seems to need `unsafe` to exist, the construct is wrong, not the ban.
