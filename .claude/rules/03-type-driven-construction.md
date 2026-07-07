# Type-Driven Construction (global)

Represent domain meaning as Rust TYPES, not as strings, bools, integer tags, set membership, or
procedural checks. This is a construction discipline for ALL first-party code, and it binds hardest
on PROOF code — verifiers, decoders, catalog readers. A verifier written with a loose runtime
taxonomy becomes a place the system can LIE while still compiling (a `verify_storage` once shipped a
`String`/`BTreeSet` taxonomy and reported a healthy store as corrupt; no gate caught it — a type
would have).

## Required

- Decode untyped bytes ONLY at the boundary; immediately construct a typed domain value
  ("parse, don't validate"). A fallible decode returns a typed `Result`, never a bare primitive.
- Dispatch by `match` on an `enum`/newtype, never by a string compare or set-membership at the
  decision site.
- Make impossible states unrepresentable where practical (an authority you cannot forge, a reader
  that cannot write, a code you cannot spend unstamped).
- A domain identity is a newtype (`RelationId`, `Epoch`, `Code`), never a bare `u64`/`u32` passed
  around as meaning.
- Tests construct through the SAME production authority (catalog/kernel) as real code — unless the
  test is EXPLICITLY a corruption/bypass test (see below).

## Forbidden

- a stringly-typed format/kind name (`kind: String`, `format: &str`) where an enum belongs;
- `BTreeSet<String>`/`HashSet<String>` (or a map) used AS the domain taxonomy at a dispatch site;
- a raw relation/index/column id (`u64`/`u32`) carrying identity without a newtype;
- verifier / codec dispatch by string compare or `.contains("literal")`;
- an unchecked constructor (`new_unchecked`, `from_raw`, `from_bytes_unchecked`, `forge`) exposed
  OUTSIDE its authority module;
- value serialization outside canonical bytes or the one ruled catalog-metadata door;
- an exact-error test weakened to "any error"; a sentinel time (`i64::MAX`) leaking through public
  semantics;
- "the fixture is unrealistic" as an excuse AFTER a verifier correctly rejects a bypassed store —
  fix the fixture to construct through production, don't blame the verifier.

## Classifying a `Map`/`Set`<`String`>

Not auto-forbidden — but a "genuine string identifier" is not a free pass. A string is allowed only
at a name/config/parse/HTTP/doc-URI boundary, or as a registry key whose VALUE is already typed and
whose string is not carrying domain semantics by itself. For every hit, ask one level deeper:

1. Is the string only an external name?
2. Is it immediately resolved to a typed domain value?
3. Does membership affect verification, dispatch, storage meaning, relation identity, index
   identity, or authority?
4. Could this be a newtype instead?

`BTreeMap<String, DataValue>` param pools, fixed-rule name → `dyn FixedRule`, HTTP headers/doc URIs,
and `parse/` token → typed enum are boundaries — fine. But if membership CONTROLS verification or
dispatch (Q3 yes), convert it to a typed key/newtype. A build-time cross-reference of
catalog-decoded names is acceptable ONLY if it is resolved at the boundary into a typed structure and
never appears at the dispatch site.

## Corruption / bypass tests (the only place raw storage writes are allowed)

- must be NAMED as a corruption/bypass test;
- must assert a TYPED corruption/refusal (the exact decode error or refusal reason), not "some error";
- must NOT double as a normal correctness proof — the healthy-path fixture is built through the real
  Db/kernel.

## The smell scan

`scripts/smell-scan.sh` is the on-demand, deliberately-noisy grep for this whole class (run it, and
`--strong` for the sharpest subset). It is NOT a gate and NOT an always-on hook — it finds
CANDIDATES; you classify every hit as `real-violation | intentional-boundary | false-positive`
against this rule. Run it when writing or reviewing proof code (verifiers, decoders, catalog
readers) and storage/test fixtures.
