---
name: rust-verbs-success
description: Build the four behavior constructs — derivation, transition, consuming verb, total refusal — the only legal method shapes in kyzo engine code. Fires before writing a free function that computes from a struct's fields, a &mut self method that mutates a field in place instead of reassigning it, a method returning Option or panicking/unwrapping on a reachable input instead of Result with a named reason and span, a self-by-value method with no reason ownership is taken, or a sealed/committed bool flag.
---

# Verbs

The behavior constructs: what a kyzo type *does*, as opposed to what it *is* (`rust-values-success`). A derivation computes a fact from a value's own already-proven fields. A transition re-points a live handle's field to a newer proven value (`rust-state-success` owns where transitions are allowed to live). A consuming verb takes `self` by value because the type itself is spent by the operation — a shape Rust's ownership model gives that has no equivalent in a garbage-collected construct catalogue. Total refusal is how every fallible domain operation answers: a typed `Result` with a named reason and span, never a panic on a reachable input. Sum-type `match` ownership (one legal match on the type's own `impl`) lives in `rust-values-success`, not here.

## Derivation

### Definition

A method taking `&self` whose body is a pure function of the value's already-proven fields: same fields, same answer, every time, no IO, no clock, no randomness.

### Required Form

```rust
impl Fill {
    pub fn exposure(&self) -> Exposure {
        Exposure::new(self.fill_price.get() * self.filled_quantity.get())
    }
}

impl Portfolio {
    pub fn total_exposure(&self) -> Exposure {
        // recursion is the whole traversal; no loop needed
        Exposure::new(
            self.exposure.get()
                + self.children.iter().map(|c| c.total_exposure().get()).sum::<Decimal>(),
        )
    }
}
```

### Sorting Rules

A fact that differs by which sum-type variant holds is that variant's own same-named method (`rust-values-success`, Sum Type), not a free function matching on the enum from outside. A question with an input beyond `&self` is a query struct (`rust-values-success`, Proven Collection's keyed-query pattern), not a parameterized derivation. A value that depends on the clock, a random source, or a live handle is not a derivation at all — it's a field, computed once where the value is born and passed into construction.

### Replaced Forms

A free function taking a struct's fields as loose parameters to compute a fact about that struct is the derivation escaped from its owner — move it onto the type as a method. A stored field that duplicates what a derivation would compute is a second copy of one fact, kept in agreement by hand instead of proven equal by construction.

### Allowed Patterns

- `&self -> DeclaredType` whose body is one expression (or a short pure block) over the value's own fields
- recursion over a recursive type as the derivation's whole traversal
- a same-named method on every variant of a sum type, called uniformly from the variant's own value

### Forbidden

- a free function or a `mod helpers`/`mod utils` function computing from a struct's public fields from outside the type
- a stored field that duplicates a derivable fact
- a derivation reading the clock, a random source, or a live client/handle
- a derivation returning bare `bool` where a two-variant sum type belongs (a staleness check returns `Fresh | Stale`, not `bool`)

### Halt Rule

Halt when the fact needs an input beyond `&self`, when it depends on something outside the value's own fields, or when it needs an operation not already available on the value's declared types. Report the type and the fact: a query struct, a field, or a missing operation is needed, and the table is not finished.

## Transition

### Definition

A `&mut self` method on a live handle (`rust-state-success`) that re-points one of its fields to a newly constructed, fully proven value. State evolution is reassignment of a proof, never mutation of a proof's contents.

### Required Form

```rust
impl Session {
    pub fn apply(&mut self, mutation: AdmittedMutation) -> Result<(), SessionError> {
        self.catalog = self.catalog.integrate(mutation)?;
        Ok(())
    }
}
```

The body constructs the new state in one expression and re-points `self.catalog` to it; it does not reach into `self.catalog`'s existing fields and mutate them piecemeal.

### Sorting Rules

A fact implied by already-proven fields, with no state change, is a derivation, not a transition. A method that only retrieves and returns a field is not a transition — consumers read facts that transitions have already established, they don't ask a method to fetch-and-return. Where a live handle is allowed to exist at all is `rust-state-success`'s doctrine, not this file's.

### Replaced Forms

A method that mutates a nested field of the current state in place (`self.catalog.generation += 1`) restates a construction as an in-place edit, leaving a half-updated value visible to anything holding a reference mid-call. A fetch-and-return method dressed as a transition (`fn catalog(&mut self) -> &Catalog`) is a repository surface given a transition's name.

### Construct-Specific Doctrine

The body constructs the new state once and assigns it once; it may also capture a foreign reply before that construction and emit the newly assigned state afterward through a held client field — but it never emits before the state is reassigned, and it never leaves `self` holding an unproven or partially-built value, even between statements.

### Allowed Patterns

- `&mut self` with a body that constructs one new proven value and reassigns it to a field
- capturing a foreign reply (one call, its declared error mapped to the same binding) before the construction
- emitting the newly assigned value through a held client field after reassignment

### Forbidden

- mutating a field's nested contents in place instead of reassigning the field to a newly constructed value
- a `&mut self` method that only fetches and returns, doing no transition
- emitting a value before the state that produced it has been assigned
- leaving `self` holding a partially constructed or unproven value between statements

### Halt Rule

Halt when the transition needs a second construction, a computation with no derivation to carry it, or a branch over raw data instead of a sum type's own method. Report the type and the statement that doesn't fit: a derivation, a missing sum-type variant, or a boundary lift is missing from the table.

## Consuming Verb

### Definition

A method taking `self` by value because the operation genuinely spends the type: the value cannot be used again after the call, and the type system — not a runtime flag — enforces that. This construct has no equivalent outside an ownership-typed language: a commit, a seal, an epoch's end.

### Required Form

```rust
pub struct Transaction { /* .. */ }

impl Transaction {
    pub fn commit(self) -> Result<CommitReceipt, Conflict> {
        // self is consumed; there is no way to call commit twice on the same handle
        // .. the compiler rejects any later use of the moved-from transaction
    }
}

pub struct BuildingIndex { /* .. */ }
pub struct SealedIndex { generation: Generation }

impl BuildingIndex {
    pub fn seal(self) -> SealedIndex {
        // consumes the building type; only the sealed type exposes search()
    }
}
```

`zone-store` states the shape directly: commits are consuming, the type is spent. `zone-project` states the sibling shape: a projection's building form and its queryable form are distinct types joined by exactly one consuming seal.

### Sorting Rules

A method that could be called again on the same value, with no change of type, is a transition (mutating a handle in place) or a derivation (no mutation at all) — not this construct. This construct exists exactly where the operation ends one phase of a type's life and produces a different type for the next phase.

### Replaced Forms

A `&mut self` method with an internal `sealed: bool` flag checked at the top of every subsequent call is the consuming verb's job done by convention instead of by the compiler — the flag can be forgotten at some future call site; a moved-from value cannot be.

### Construct-Specific Doctrine

A conflict, a refusal, or a corruption on a consuming verb is a typed `Result`, never a panic — the type is spent either way, but the caller is told which of two outcomes happened, never left assuming success.

### Allowed Patterns

- `pub fn verb(self) -> Result<NextPhaseType, TypedRefusal>` where `NextPhaseType` differs from `Self`
- a distinct type per lifecycle phase (building vs. sealed, open transaction vs. committed) joined by exactly one consuming method
- an unforgeable "spent" guarantee coming from ownership, never from a runtime flag field

### Forbidden

- a `sealed: bool`/`committed: bool` flag checked by convention where a consuming verb and a distinct next-phase type belong
- a consuming verb that panics on conflict or corruption instead of returning a typed `Result`
- a method that both consumes `self` and returns `Self` unchanged — that's not a phase change, it's a `&mut self` transition with extra ceremony

### Halt Rule

Halt when a lifecycle boundary is being enforced by a runtime flag instead of a type change, or when the next-phase type doesn't yet exist. Report the type and the phase: the sealed/committed/consumed form is missing from the table.

## Total Refusal

### Definition

Every fallible domain operation — a smart constructor, a transition that can conflict, a consuming verb that can refuse, a decode already owned by `rust-adapters-success` — returns `Result<T, E>` where `E` is a typed refusal naming its reason and, where a source location exists, its span. A domain-reachable input never ends in `panic!` / `.unwrap()` / `.expect()`.

### Required Form

```rust
pub enum AdmitError {
    Constraint { constraint: ConstraintId, rows: Vec<RowId> },
    Decode { reason: DecodeReason, span: Span },
}

impl Session {
    pub fn admit(&mut self, raw: RawMutation) -> Result<AdmittedMutation, AdmitError> {
        let mutation = AdmittedMutation::try_from(raw)?; // typed refusal propagates; no unwrap
        self.catalog = self.catalog.integrate(mutation.clone())?;
        Ok(mutation)
    }
}
```

Boundary decode owns totality over bytes (`rust-adapters-success`); this construct owns the method-shape law for every fallible verb and constructor the engine exposes: the error type is structured, matchable, and carries reason + span — never a bare `String` or `anyhow::Error` at a domain boundary.

### Sorting Rules

An operation that cannot fail on any domain-reachable input is not this construct — return the value directly (a derivation, an infallible ctor over already-proven inputs). An operation that fails only when the programmer violated an API contract (a drop-bomb on an unfinished transaction) is `rust-state-success`'s Drop-Bound Resource, not a total refusal to the caller.

### Replaced Forms

`.unwrap()` / `.expect()` / `panic!` on a `Result` from foreign or caller-supplied input turns a typed refusal into a crash. Returning `Option` for a failure that has a reason discards the reason. A `bool` "ok" flag next to an out-parameter is a refusal with no type.

### Allowed Patterns

- `-> Result<T, E>` where `E` is a domain enum with named reasons (and `span` where structural)
- `?` propagation of typed refusals through transitions and consuming verbs
- infallible returns only when every input path is already branded-proven

### Forbidden

- `.unwrap()` / `.expect()` / `panic!` on a domain-reachable failure path
- `Option<T>` or `bool` standing in for a refusal that has a reason
- `Result<T, String>` / `Result<T, anyhow::Error>` at an engine or host boundary

### Halt Rule

Halt when a fallible operation has no typed error enum, or when a call site can only surface failure by panicking. Report the operation and the missing reason: the refusal type is not yet modeled, and the table is not finished.
