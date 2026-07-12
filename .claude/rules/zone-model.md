---
paths:
  - "crates/kyzo-model/**/*.rs"
  - "crates/kyzo-model/**/*.pest"
---

# Zone: Model — the shared vocabulary

What the engine, its judges, and its hosts must all agree on before execution
exists: values, schemas, programs, the parse lift, the wire envelopes.

## Required

- Pure data only. The zone's verbs are construct, compare, encode, decode.
- Every decode returns a typed `Result`; every refusal names its reason and span.
- Every parse and decode is total over every byte sequence: recursive descent
  carries a typed depth bound, no input-declared size reaches an allocator
  unbounded or as an infallible reservation, and no matching path admits
  catastrophic backtracking. Adversarial totality is proven by fuzzing, never
  asserted by review.
- Every value kind defines its identity and order law BEFORE its bytes; the
  canonical encoding is the ONLY byte authority for values, everywhere.
- Vector dimensionality is schema- and data-determined per row, never pinned
  into the value type as a const generic — the value plane cannot carry it as
  a type-level fact; where a kernel needs a fixed dimension, that guarantee is
  minted once at admission and consumed downstream without re-checking.
- Constructors enforce identity laws; unchecked constructors are private to
  their module; compile-time absence proofs seal the doors.
- A value lifted at a boundary carries its proof forward as a branded fact; no
  downstream site re-checks what a constructor already proved — a redundant
  re-check is dead code or a missing type.
- One `Tag` is the only type-discriminant and cross-type order authority;
  comparison flows through the one prefix doctrine.
- `Expr` and the program tiers define MEANING as data — no evaluator here.
- Wire codecs (JSON, Arrow) are total, round-trip-proven views; they add no
  meaning the value plane lacks.
- The grammar advertises nothing unowned: every grammar rule has an owner or
  an explicitly owned typed refusal.
- The model's order is STRUCTURAL (storage and identity); query-semantic
  comparison belongs to evaluation and is defined separately — the two must
  never be conflated at any site.
- Unboundedness is a shape, not a sentinel: an unbounded endpoint is a
  distinct typed variant, byte-distinct and round-trip-distinct from every
  finite instant. No sentinel value ever carries meaning, and no sentinel
  leaks through a public API.

## Forbidden

- IO, clocks, randomness, storage access, evaluation, allocation-heavy cleverness.
- A second serialization of any value, anywhere, for any reason.
- String-typed names or kinds surviving past the parse boundary.
- Dependencies on any other crate of ours.
- Changing a RELEASED byte format without a FormatVersion decision and
  round-trip + ordering tests: byte order MUST equal semantic value order.
