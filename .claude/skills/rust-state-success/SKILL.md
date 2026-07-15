---
name: rust-state-success
description: Build kyzo's zone-native live-state constructs — capability handle, species pair, building-to-sealed lifecycle, epoch-scoped currency, drop-bound resource — the only shapes mutable state and transport/storage handles are allowed to take. Fires before holding a cursor, transaction, lock, or client as a struct field; before writing a manager/engine/service struct with an interior-mutability field added ad hoc; before writing a reader/writer pair with the invariant checked at runtime; before letting an index answer a search before it's finished building; or before an interned code, row, or column that might outlive its evaluation epoch.
---

# State

The live-node constructs: where mutable state and live handles are allowed to exist at all. Everything in `rust-values-success` is immutable data; kyzo has no single god ConsistencyModel the way a single bounded service would — it has several zone-native lifecycle shapes, each solving one form of "when is mutation, or a live handle, legal."

## Capability Handle

### Definition

One live handle per seated truth (a transaction, a live storage session, a subscription), holding concrete clients/resources as fields and evolving its state by reassignment to a newly constructed value (`rust-verbs-success`, Transition) — never by ad hoc interior mutability sprinkled across unrelated types.

### Required Form

```rust
pub struct Session {
    store: StoreHandle,
    catalog: Catalog,
    observers: ObserverSet,
}

impl Session {
    pub fn apply(&mut self, mutation: AdmittedMutation) -> Result<(), SessionError> {
        self.catalog = self.catalog.integrate(mutation)?;
        Ok(())
    }
}
```

### Sorting Rules

A second unfrozen/mutable struct converging the same state as an existing handle means the context is two contexts — stop and report it, don't add a second handle for the same truth. A fact the state implies is a derivation on the state's own type (`rust-verbs-success`), never a method here.

### Replaced Forms

A "manager" or "engine" struct name given to what is actually domain logic with no proof obligation is this construct wearing a technology name. A module-level `static`/`lazy_static` client is the live edge escaped from the one handle that should hold it.

### Construct-Specific Doctrine

Interior mutability (`RefCell`, `Mutex`, `RwLock`) is legal only where genuine shared/concurrent access is a real requirement **and** the owning zone's determinism law permits it. It is never a substitute for `&mut self` reassignment on a single-owner type, and never a habit on `zone-exec` / `zone-store` paths where nondeterministic lock ordering or shared mutation would affect committed or evaluated state. Prefer ownership and reassignment; escalate to interior mutability only with a stated concurrency need that does not violate zone law.

### Allowed Patterns

- one struct per seated truth, concrete clients as fields, state fields as declared types from `rust-values-success`
- state evolution by reassigning a field to a newly constructed value
- `RefCell`/`Mutex`/`RwLock` only with a stated shared-access requirement that zone determinism still permits

### Forbidden

- a second capability handle converging the same state as an existing one
- a "manager"/"engine"/"service"-named struct that is domain logic with no proof obligation
- a module-level `static` holding a live client
- `RefCell`/`Mutex` added to a single-owner, non-concurrent struct as a workaround for the borrow checker instead of `&mut self`
- interior mutability on deterministic eval/store paths where lock order or shared mutation can affect answers or committed state

### Halt Rule

Halt when a context needs a second live handle for what looks like the same truth, or when a client no transition reaches is demanded. Report the handle and the client: the context is mis-factored or the meaning is unmodeled, and the table is not finished.

## Species Pair

### Definition

Where an operation's legality (read vs. write, at minimum) is a domain invariant, that invariant is held by distinct TYPES — a reader type simply has no write method to call — never by a runtime flag or a documented convention checked by discipline alone.

### Required Form

```rust
pub struct ReadCursor<'txn> { /* .. */ }
pub struct WriteCursor<'txn> { /* .. */ }

impl WriteCursor<'_> {
    pub fn put(&mut self, key: &[u8], value: &[u8]) {
        // only WriteCursor has this method
    }
}
```

`zone-store` states it directly: species invariants held by TYPES — a reader cannot write; never move an invariant down the enforcement ladder to runtime checks or convention.

### Sorting Rules

If every consumer of a type is trusted never to call a hypothetical write method, that trust is exactly the runtime-convention enforcement this doctrine forbids — split the type the moment two different capability levels exist for the same underlying resource.

### Replaced Forms

One `Cursor` struct with an internal `read_only: bool` checked at the top of `put()` is the species invariant enforced by convention instead of by the compiler — the flag can be constructed wrong, or the check forgotten in a new method.

### Construct-Specific Doctrine

A cursor's validity is tied by type to the snapshot or transaction it was opened against (below, Drop-Bound Resource); a cursor that outlives its snapshot is a borrow-check error at compile time, never a runtime guard checked on each call.

### Allowed Patterns

- distinct types per capability level (reader/writer, at minimum), each exposing only the methods its level legitimately has
- a lifetime parameter tying a cursor's validity to its owning transaction/snapshot

### Forbidden

- one type with a `read_only`/`can_write`/`mode` flag checked inside methods instead of a type split
- a cursor type with no lifetime tying it to the snapshot/transaction it was opened against

### Halt Rule

Halt when a capability distinction is being enforced by a flag or a doc comment instead of a type split. Report the resource and the two capability levels: the species pair is missing, and the table is not finished.

## Building-to-Sealed Lifecycle

### Definition

A projection, index, or other rebuildable structure has a building form and a queryable form as distinct types, joined by exactly one consuming verb (`rust-verbs-success`): the building type exposes no search/query method at all, and the sealed generation is part of the type-visible contract.

### Required Form

```rust
pub struct BuildingIndex { rows: Vec<Row> }
pub struct SealedIndex { generation: Generation, rows: Vec<Row> }

impl BuildingIndex {
    pub fn seal(self) -> SealedIndex {
        SealedIndex { generation: Generation::next(), rows: self.rows }
    }
}

impl SealedIndex {
    pub fn search(&self, query: &Query) -> Candidates {
        // only the sealed form can be searched
    }
}
```

`zone-project` states it directly: querying an unsealed index is absent from the interface, not a runtime error.

### Sorting Rules

This is a lifecycle distinction (state-of-construction over time), so it belongs here even though the mechanism — two structurally similar structs joined by a consuming method — looks like `rust-values-success`'s Sum Type. The difference: a sum type is a choice among structures at one instant; this is a phase change over time, with different methods legal per phase and no way back.

### Replaced Forms

One `Index` struct with a `sealed: bool` and a `search()` method that returns an error (or empty results) when called on an unsealed instance is the same runtime-convention failure as the species pair, applied to a lifecycle instead of a capability level.

### Construct-Specific Doctrine

Every projection is rebuildable byte-identically from canonical facts at any time (`zone-project`'s projection law); if a `BuildingIndex`/`SealedIndex` pair can't be regenerated from store facts alone, the projection has quietly become an authority, which is a red gate, not a state-construct concern to patch here.

### Allowed Patterns

- two distinct types (building, sealed) per rebuildable structure, joined by exactly one consuming `seal()` method
- the sealed type's generation stamp as part of its type-visible contract
- a decode failure crossing an engine boundary rendered as a typed engine-corruption error, never a raw decode error leaking through

### Forbidden

- one type with a `sealed`/`ready` flag gating a `search()` method that errors or returns nothing when false
- a `search()`-shaped method reachable on the building type at all
- a projection type that cannot be regenerated byte-identically from canonical store facts

### Halt Rule

Halt when a structure's build/query phases are distinguished by a flag instead of a type, or when the sealed form can't be proven regenerable from canonical facts. Report the structure and the phase: the lifecycle split (or the projection law itself) is not yet satisfied.

## Epoch-Scoped Currency

### Definition

An execution-local identity (an interned code, a row, a column) that is unforgeable (no public raw constructor) and dies with its evaluation epoch: nothing serializes it, and no instance of it leaves the epoch that minted it — a fact `rust-adapters-success`'s Wire Envelope construct never gets asked to carry.

### Required Form

```rust
pub struct Code(u32); // private field

impl Code {
    pub(crate) fn intern(value: &Value, epoch: &Epoch) -> Self {
        // only mintable inside an epoch, no pub constructor
    }
}
```

`zone-exec` states it directly: codes are unforgeable, no public raw constructors; the execution currency never persists — codes, rows, and columns die with their epoch; nothing serializes them and no code leaves an encoded key.

### Sorting Rules

The moment a value needs to persist past one evaluation, it is not this construct — it is a canonical value (`rust-values-success`/`rust-order-success`), re-derived from durable bytes on the next epoch, never carried forward as a code.

### Replaced Forms

A `pub fn Code::from_raw(u32) -> Code` "for testing convenience" is the unforgeable guarantee undone by the one call site that needed it least; every raw-code compare or spend must happen under same-domain admission, with no public door around that rule.

### Construct-Specific Doctrine

Canonical encoding never runs in the hot loop this currency exists to avoid (`zone-exec`'s counter law); a code that gets encoded back to canonical bytes mid-evaluation "just this once" reintroduces the cost this construct was minted to eliminate.

### Allowed Patterns

- a private-field wrapper with a `pub(crate)`-or-narrower minting function scoped to one epoch
- comparison/dedup operating only on codes minted within the same epoch's admission domain
- the code discarded (not serialized, not returned across an epoch boundary) when the epoch ends

### Forbidden

- a public raw constructor for an interned code
- a code, row, or column serialized, cached, or returned across an epoch boundary
- canonical encoding invoked in the per-row hot path to "double check" a code

### Halt Rule

Halt when a value needs to outlive its evaluation epoch. Report the value: it belongs to `rust-values-success`/`rust-order-success` as a canonical, durable type instead, and the table is not finished.

## Drop-Bound Resource

### Definition

Every acquired resource whose validity is scoped — an open cursor, a lock, a scratch store, a live transaction — releases through `Drop` bound to that scope. An unfinished protocol whose silent drop would corrupt an invariant is a drop-bomb: a safe terminal default, or a named panic, but never a forgettable no-op.

### Required Form

```rust
pub struct WriteTransaction<'db> { /* .. */ }

impl Drop for WriteTransaction<'_> {
    fn drop(&mut self) {
        if !self.finished {
            // silent drop would corrupt an in-flight write: name it loudly
            panic!("WriteTransaction dropped without commit() or abort() — this is a bug, not a recoverable error");
        }
    }
}
```

### Sorting Rules

A resource whose silent drop is genuinely harmless (a read-only cursor with nothing to flush) needs no drop-bomb — an ordinary `Drop` releasing the resource is enough. The drop-bomb is reserved for the case where silence would corrupt state, not applied reflexively to every `Drop` impl.

### Replaced Forms

A resource released "eventually" by a background sweep, or relying on the caller remembering to call `.close()`/`.commit()` before the value goes out of scope, is exactly the forgettable call site this construct exists to close off — `Drop` makes the release unforgettable by construction.

### Construct-Specific Doctrine

A cursor's `Drop`-bound scope is the same scope its species-pair lifetime already ties it to (above); the two constructs work together — the lifetime prevents it outliving its transaction at compile time, `Drop` handles what happens when its own scope ends.

### Allowed Patterns

- `Drop` releasing a scoped resource unconditionally when release is harmless
- a drop-bomb (loud panic, or a safe terminal default explicitly chosen and documented) when an unfinished protocol's silent drop would corrupt an invariant
- a lifetime tying a resource's validity to the scope whose `Drop` governs it

### Forbidden

- a resource released by a background sweep, timeout, or "eventually" mechanism instead of `Drop`
- an unfinished protocol (an open transaction with pending writes, a lock not yet released) that drops silently with no drop-bomb and no safe terminal default
- relying on callers remembering to call a manual close/finish method before scope end

### Halt Rule

Halt when a resource's release can't be tied to a `Drop` impl, or when an unfinished protocol has no named safe default or drop-bomb. Report the resource and the unfinished state: the release discipline is not yet modeled, and the table is not finished.
