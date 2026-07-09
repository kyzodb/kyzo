---
paths:
  - "kyzo-model/**/*.rs"
  - "kyzo-model/**/*.pest"
---

# Zone: Model — the shared vocabulary

What the engine, its judges, and its hosts must all agree on before execution
exists: values, schemas, programs, the parse lift, the wire envelopes.

## Required

- Pure data only. The zone's verbs are construct, compare, encode, decode.
- Every decode returns a typed `Result`; every refusal names its reason and span.
- Every value kind defines its identity and order law BEFORE its bytes; the
  canonical encoding is the ONLY byte authority for values, everywhere.
- Constructors enforce identity laws; unchecked constructors are private to
  their module; compile-time absence proofs seal the doors.
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
