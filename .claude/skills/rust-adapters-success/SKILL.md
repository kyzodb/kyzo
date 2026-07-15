---
name: rust-adapters-success
description: Build the three boundary-crossing constructs — boundary decode, wire envelope, ordered TryFrom lift — the only shapes for data crossing into or out of the kyzo engine. Fires before touching bytes from disk, a network request, a foreign file format, or an FFI/WASM boundary; before writing a parser or grammar rule; or before writing unwrap/expect on external input, a hand-rolled byte-indexing parser, or an untyped String error at a boundary.
---

# Adapters

The boundary constructs: how bytes and foreign shapes are trusted crossing in, and how domain values are rendered crossing out. Distinct from `rust-values-success`, whose constructs represent truth already inside the engine, and from `rust-order-success`, which proves the byte order those crossings must respect.

## Boundary Decode

### Definition

A decode function or `TryFrom`/`TryInto` impl that is TOTAL over every byte sequence: it returns a typed `Result` for every input, never panics, and every refusal names its reason and, where a grammar is involved, its span.

### Required Form

```rust
pub fn decode(bytes: &[u8]) -> Result<Value, DecodeError> {
    if bytes.is_empty() {
        return Err(DecodeError::Truncated { at: 0 });
    }
    let tag = Tag::try_from(bytes[0])
        .map_err(|_| DecodeError::UnknownTag { at: 0, byte: bytes[0] })?;
    // every subsequent read is bounds-checked and every failure named
    todo_decode_payload(tag, &bytes[1..])
}
```

Recursive descent (nested value decoding) carries a typed depth bound, checked before each recursive call, so no adversarial input can exhaust the stack; no input-declared size (a length prefix) reaches an allocator as an unbounded or infallible reservation before it's validated against the remaining input length.

### Sorting Rules

If the incoming shape already matches a domain type exactly, decode constructs that type directly — no intermediate "wire struct" exists purely to be immediately unwrapped. Data expected to sometimes legitimately fail (a corrupt frame, an unparseable tail) constructs through an ordered lift (below), not through a decode that treats "fails sometimes" as an error case.

### Replaced Forms

A hand-rolled byte-indexing loop (`bytes[i]`, `bytes[i+1]`, ad hoc) scattered across a function restates what a total decoder should own in one place; a slice-indexing panic on out-of-range input is exactly the adversarial-totality failure this doctrine forbids. `.unwrap()`/`.expect()` anywhere in a decode path assumes the input is trusted, which is precisely the thing crossing a boundary is not.

### Construct-Specific Doctrine

Adversarial totality — every byte sequence decodes to either `Ok` or a named `Err`, never a panic, never an infinite loop, never a catastrophic-backtracking blowup — is proven by fuzzing, never asserted by review (`zone-model`). A decoder with no fuzz target is a decoder whose totality is a claim, not a fact.

Every grammar rule has an owner or an explicitly owned typed refusal (`zone-model`): a parser combinator or `pest` rule with no corresponding variant in the error type is advertising a case nothing handles.

### Allowed Patterns

- `fn decode(&[u8]) -> Result<T, TypedError>` total over all inputs, bounds-checked, every failure named with reason (and span, where structural)
- a typed, checked recursion depth bound on any self-referential grammar
- a length-prefixed field validated against remaining input length before any allocation sized by it
- a fuzz target exercising the decoder against arbitrary bytes
- every grammar rule paired with an owning success case or an explicitly owned refusal variant

### Forbidden

- `.unwrap()`/`.expect()`/direct slice indexing (`bytes[i]`) with no bounds check, anywhere a decode reads untrusted input
- recursive descent with no depth bound
- an allocation sized directly by an input-declared length with no validation against remaining input size first
- a decoder shipped with no fuzz target
- a grammar rule that resolves to no typed case, success or refusal

### Halt Rule

Halt when a byte sequence cannot be classified as `Ok` or a named typed refusal, when a recursive rule has no depth bound, or when a grammar rule owns nothing. Report the shape and the byte sequence: the decoder is not yet total, and the table is not finished.

## Wire Envelope

### Definition

A `serde`-derived struct (or Arrow schema) that is a total, round-trip-proven VIEW of an existing domain value for one wire format (JSON, Arrow) — it adds no meaning the value plane lacks, and it is never a second place a value's meaning is decided.

### Required Form

```rust
#[derive(Serialize, Deserialize)]
pub struct FillWire {
    order_id: String,
    account_id: String,
    fill_price: String, // decimal serialized as a string; parsed back through Price::new
    filled_quantity: String,
}

impl From<&Fill> for FillWire { /* .. */ }
impl TryFrom<FillWire> for Fill { /* re-validates through the same constructors as any other input */ }
```

### Sorting Rules

This program's own request/reply shape at a host boundary (`rust-wiring-success`'s Sealed Public Door) is this construct; a value already representable directly by a domain type needs no wire twin at all — serialize the domain type itself.

### Replaced Forms

A second serialization path for any value, anywhere, for any reason, is forbidden outright by `zone-model` — a second serialization of any value is named as a standing violation, not a case-by-case judgment call. A wire struct that becomes a second source of truth (fields added to the wire shape that the domain type doesn't have, silently carried through) is meaning smuggled past the ontology.

### Construct-Specific Doctrine

Round-trip is proven, not assumed: a property test constructs a domain value, serializes it to the wire shape, deserializes back, and asserts equality — for every wire format the value crosses.

### Allowed Patterns

- one `serde`/Arrow wire struct per domain type per format, existing only where the wire shape genuinely differs from the domain shape
- `From`/`TryFrom` conversions between the domain type and its wire twin, re-validating through the domain type's own constructors
- a round-trip property test per wire format

### Forbidden

- a second encoding of any value that competes with `rust-order-success`'s canonical encoding as a byte authority
- a wire struct carrying a field the domain type doesn't have
- a wire shape assumed to round-trip with no test proving it

### Halt Rule

Halt when a wire shape would need to carry meaning the domain type doesn't have, or when two encodings of the same value would coexist. Report the value and the format: either the domain type is missing a field or a second byte authority is being created, and the table is not finished.

## Ordered TryFrom Lift

### Definition

`TryFrom`/an ordered attempt sequence for foreign data expected sometimes to legitimately fail or say no — the stronger construction attempted first, a named failure case last, composing from the input itself.

### Required Form

```rust
pub enum Frame {
    Tick(Tick),
    Unparseable(UnparseableFrame),
}

impl TryFrom<&[u8]> for Frame {
    type Error = Infallible; // the failure arm IS a variant, never a raised error
    fn try_from(bytes: &[u8]) -> Result<Self, Infallible> {
        match Tick::try_from(bytes) {
            Ok(tick) => Ok(Frame::Tick(tick)),
            Err(_) => Ok(Frame::Unparseable(UnparseableFrame::from_raw(bytes))),
        }
    }
}
```

Fed bytes that parse, `Tick` constructs; fed garbage, `Tick` refuses and `Unparseable` composes from the same input. As a field, `frame: Frame` admits garbage as a declared value — never as a propagated error.

### Sorting Rules

One question routes every failure: did the domain say no (a value — this construct), or did a proof fail (a bug or an adversarial input — this propagates as an `Err`, never silently swallowed into a default)? Identity-carrying data (a discriminant tag is present) constructs through the sum type's own discriminant (`rust-values-success`), never by attempt order.

### Replaced Forms

A `match`/inspection function that peeks at a foreign reply's shape to decide which variant to build restates the selection the ordered `TryFrom` chain already performs. A `.unwrap_or_default()`/`x.unwrap_or(fallback)` on a fallible foreign parse manufactures a value nothing proved.

### Construct-Specific Doctrine

The one legal fallback: the failure variant composes from the raw input itself (bytes, string, whatever arrived), never from a synthesized default unrelated to what actually arrived. A caught foreign exception/error type is captured once, at the earliest possible boundary call, and immediately fed into the ordered construction — never inspected, branched on, or partially handled first.

### Allowed Patterns

- an ordered attempt sequence (strongest construction first, named failure variant last) as an enum's `TryFrom` impl
- the failure variant composing from the raw input that arrived
- a captured foreign `Result::Err` fed directly into the ordered construction, one call, one binding

### Forbidden

- `.unwrap_or_default()`/`.unwrap_or(fallback)` on a fallible foreign parse
- a reply-inspection function or `match` that decides a foreign reply's case before ordered construction runs
- a failure variant synthesizing a default value instead of composing from the actual input
- catching a foreign error and branching on its contents before ordered construction

### Halt Rule

Halt when a foreign source produces a signal or marker not named by its own interface, or when the failure variant cannot compose from the input itself. Report the source and the signal: the wire's shapes are not fully modeled, and the table is not finished.
