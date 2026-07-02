---
paths:
  - "kyzo-lib-c/**"
  - "kyzo-lib-java/**"
  - "kyzo-lib-nodejs/**"
  - "kyzo-lib-python/**"
  - "kyzo-lib-swift/**"
  - "kyzo-lib-wasm/**"
---
# Rule: FFI / unsafe boundary (the language bindings)

The FFI and `unsafe` surface is the six language bindings, each with `unsafe` code and a foreign
toolchain:

- `kyzo-lib-c` — C ABI (`extern "C"` + `cbindgen`)
- `kyzo-lib-python` — `pyo3` (CPython)
- `kyzo-lib-java` — `jni` (JVM)
- `kyzo-lib-nodejs` — `neon` / napi (Node)
- `kyzo-lib-swift` — `swift-bridge`
- `kyzo-lib-wasm` — `wasm-bindgen`

- None of these are covered by the core CI. A green core build says nothing about them.
- The boundary is typed on both sides: kernel/engine errors are typed values (retryable conflict,
  fatal corruption, resource limits) and each binding maps them to the host language's native error
  types — never to strings a caller must parse.
- Unsafe/FFI changes need an explicit invariant review: ownership and lifetimes across the boundary,
  null / UB, and foreign-error-to-`Result` translation.

**A change here requires:** building the affected binding with its own toolchain, plus an
unsafe-invariants review (see the unsafe-ffi reviewer agent).
