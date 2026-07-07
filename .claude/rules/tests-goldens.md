---
paths:
  - "kyzo-core/tests/**/*.rs"
  - "kyzo-core/src/**/tests.rs"
  - "kyzo-core/benches/**/*.rs"
  - "**/*golden*"
  - "**/*fixture*"
---

# Tests and Goldens

Tests are proof, not decoration. Never weaken a test to make the suite green.

## When a test fails after a semantic migration, classify it

1. **The test encodes old false behavior.** Replace it with a STRONGER test for the new law, and
   document the semantic ruling. (A weaker test is not a replacement.)
2. **The implementation violates the new law.** Fix the implementation.
3. **The fixture speaks deleted vocabulary.** Migrate the fixture. Do not add a compatibility shim.

## Forbidden

- changing an exact error assertion to "any refusal"
- deleting a corruption-type expectation
- updating a fingerprint/golden by copying implementation output
- marking a test `#[ignore]` without a ledger (`01-no-deferral.md`)
- broadening an assertion unless the old assertion was provably wrong

## Goldens are independently derived

A golden copied from current implementation output is INVALID. Every golden must be independently
derived from the format law: by hand derivation, an independent test-only encoder, or a byte-by-byte
comment. A mutation-test the harness (assert absence of a trait the type has, and confirm it fails to
compile) proves a compile-fail proof has teeth.
