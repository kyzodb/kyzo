---
name: rust-adapters-failure
description: Fires when a kyzo boundary crossing is about to be hand-rolled instead of built as the matching construct in rust-adapters-success — unwrap/expect/panic! on external input, a hand-indexed byte parser with no bounds check, a second serialization path for a value that already has one, a reply-inspection function choosing a variant before ordered construction, or an untyped/unnamed refusal with no reason or span.
---

# Adapters — failure patterns

Ways a boundary crossing gets hand-rolled instead of built as a boundary decode, wire envelope, or ordered `TryFrom` lift (`rust-adapters-success`).

## Panic on untrusted input

`.unwrap()`/`.expect()` on a parse or lookup driven by external bytes assumes the input is already trusted — exactly the thing a boundary crossing is not.

```rust
let tag = Tag::try_from(bytes[0]).unwrap(); // bytes[0] is untrusted: propagate a typed DecodeError naming the unknown byte and its offset
```

## Unbounded recursion or allocation

A recursive-descent decoder with no depth bound, or an allocation sized directly from an input-declared length with no validation, lets adversarial input exhaust the stack or the heap.

```rust
fn parse_expr(bytes: &[u8]) -> Expr {
    // no depth counter: a deeply nested input blows the stack before it's ever proven invalid
}

let mut buf = Vec::with_capacity(declared_len); // declared_len came from the input, unchecked against remaining bytes
```

## Second serialization path

A hand-written encode/decode that duplicates what the canonical encoding (`rust-order-success`) or an existing wire envelope already does creates a second byte authority for the same value — forbidden outright, never a case-by-case judgment call.

```rust
// a bespoke to_json() alongside the already-existing FillWire round-trip:
// two authorities for one value's wire shape, free to drift the moment one changes
```

## Reply-inspection before construction

A function that peeks at a foreign reply's shape or fields to decide which variant to build restates the selection an ordered `TryFrom` chain should perform through construction itself.

```rust
fn classify(raw: &RawReply) -> ReplyKind {
    if raw.error_code.is_some() { ReplyKind::Error } else { ReplyKind::Ok } // this IS the TryFrom chain's job: build the ordered enum and let construction select
}
```

## Untyped error at a boundary

A `String`/`anyhow::Error`-typed refusal with no structured reason or span forces every caller to string-match instead of matching on a typed variant.

```rust
fn decode(bytes: &[u8]) -> Result<Value, String> { // "invalid byte at position 3": a typed DecodeError enum with a named variant and a span field instead
```

## Unowned grammar rule

A grammar rule with no corresponding success type and no corresponding typed refusal variant is a case nothing in the error type or the domain accounts for.

```rust
// pest rule `comment` parses but no AST variant consumes it and no error variant
// names what happens if it's malformed: give it an owner or an explicit typed refusal
```

## Sentinel default on parse failure

A `.unwrap_or_default()`/`.unwrap_or(fallback)` on a fallible foreign parse manufactures a value nothing in the domain actually proved, silently masking a refusal as success.

```rust
let quantity = Quantity::try_from(raw_field).unwrap_or(Quantity::ZERO); // masks a real parse failure as a legitimate zero: propagate the Err, or model the failure as a named variant
```

## Standing ban: `unsafe`

`#![forbid(unsafe_code)]` applies repo-wide. `unsafe` is never a legal shortcut here — not to transmute untrusted bytes directly into a typed value, not to skip bounds checks in a decoder for speed. If a boundary crossing seems to need `unsafe` to exist, the construct is wrong, not the ban.
