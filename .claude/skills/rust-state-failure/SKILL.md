---
name: rust-state-failure
description: Fires when kyzo mutable state or a live handle is about to land somewhere wrong instead of the matching construct in rust-state-success — a second capability handle converging the same state, RefCell/Mutex added to a single-owner struct as a borrow-checker workaround, a read/write or sealed/unsealed distinction enforced by a bool flag instead of a type split, a public raw constructor for an interned code, a code/row/column serialized past its epoch, or an unfinished protocol (open transaction, held lock) with no Drop-bound release.
---

# State — failure patterns

Ways mutable state or a live handle lands wrong instead of built as a capability handle, species pair, building-to-sealed lifecycle, epoch-scoped currency, or drop-bound resource (`rust-state-success`).

## Second convergence point

A second mutable struct holding overlapping state for what is really one context is two competing sources of truth for the same live fact.

```rust
struct Session { catalog: Catalog, .. }
struct CatalogCache { last_known: Catalog, .. } // a second convergence point for the same truth: one handle, or the contexts are genuinely different — name which
```

## `RefCell`/`Mutex` as a borrow-checker workaround

Wrapping a field in `RefCell`/`Mutex` on a single-owner, non-concurrent type sidesteps the borrow checker instead of using `&mut self` reassignment, and hides a mutation the compiler could otherwise have checked. The same smell applies when interior mutability is dropped onto a deterministic exec/store path without a stated concurrency need the zone law permits.

```rust
struct Session {
    catalog: RefCell<Catalog>, // no concurrent access here — nothing justifies this: &mut self and reassignment
}
```

## Bool-flag species/lifecycle

A `read_only`/`sealed`/`mode` flag checked inside methods, instead of a type split, lets an invalid operation compile and only fail (or worse, silently corrupt) at runtime.

```rust
struct Cursor {
    read_only: bool,
}

impl Cursor {
    fn put(&mut self, k: &[u8], v: &[u8]) {
        if self.read_only { panic!("read-only"); } // ReadCursor/WriteCursor as distinct types makes this call not compile instead
    }
}
```

## Forgeable code

A public raw constructor for an interned execution code lets a caller mint a code outside the epoch's admission domain, breaking the unforgeability guarantee the whole currency depends on.

```rust
impl Code {
    pub fn from_raw(v: u32) -> Self { Self(v) } // public and unchecked: pub(crate) intern(), scoped to one epoch's admission
}
```

## Currency escaping its epoch

An interned code, row, or column serialized, cached, or returned past the evaluation that minted it carries a meaning that no longer has an admission domain to be compared against.

```rust
struct CachedPlan {
    codes: Vec<Code>, // codes from a prior epoch, held across evaluations: codes die with their epoch — cache the canonical values instead
}
```

## Silent-drop corruption

An unfinished protocol (an open write transaction, a held lock) that drops with no `Drop` impl, or a `Drop` impl that silently no-ops instead of naming the corruption, loses the failure the moment the process moves on.

```rust
struct WriteTransaction { finished: bool }
// no Drop impl at all: dropping mid-write silently leaves whatever partial state existed —
// add Drop with a drop-bomb (panic or a named safe terminal default) when !finished
```

## Standing ban: `unsafe`

`#![forbid(unsafe_code)]` applies repo-wide across every `rust-*` group. `unsafe` is never a legal shortcut for any construct here. If a state construct seems to need `unsafe` to exist, the construct is wrong, not the ban.
