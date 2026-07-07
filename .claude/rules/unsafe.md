---
paths:
  - "kyzo-core/src/lib.rs"
  - "kyzo-bin/src/main.rs"
  - "scripts/check-unsafe.sh"
  - "kyzo-core/src/data/value/**/*.rs"
---

# Unsafe Policy

First-party Kyzo value and authority code FORBIDS unsafe. The engine crate roots declare:

    #![forbid(unsafe_code)]

- No phantom exceptions. No reserved future unsafe zone. No `GermanStr` unsafe exception — GermanStr
  is a safe wrapper over the 16-byte value cell; the value plane is pure safe Rust.
- `forbid` (unlike `deny`) cannot be locally lifted by any `#[allow(unsafe_code)]`.
- `scripts/check-unsafe.sh` enforces the lint, the zero-`allow` rule, AND that no doc claims an
  exception that does not exist (a lying guard is itself a failure).

## Introducing unsafe (if ever)

A future PR that genuinely needs unsafe must explicitly lower the rule at the NARROWEST possible
scope and carry a proof-quality safety case:

- exact file / function / block
- the exact invariant Rust cannot prove, and why safe Rust cannot express it
- why the unsafe is required (for correctness, or measured performance with benchmark evidence)
- Miri / audit result where applicable
- a safe wrapper that prevents misuse, and tests proving the wrapper boundary
- explicit approval in the story text

Until such a PR exists, unsafe does not exist in governed first-party code. The bindings (C ABI,
pyo3, jni, neon, swift-bridge, wasm-bindgen) are the separate FFI zone (see `ffi-bindings.md`).
