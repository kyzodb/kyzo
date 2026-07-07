---
paths:
  - "kyzo-lib-c/**"
  - "kyzo-lib-java/**"
  - "kyzo-lib-nodejs/**"
  - "kyzo-lib-python/**"
  - "kyzo-lib-swift/**"
  - "kyzo-lib-wasm/**"
---

# FFI Boundary (the language bindings)

The bindings (C ABI, pyo3, jni, neon, swift-bridge, wasm-bindgen) are the ONE `unsafe`/FFI zone —
separate from `#![forbid(unsafe_code)]` first-party code. A green core build says nothing about them.

- The boundary is typed on BOTH sides: engine errors are typed values (retryable conflict, fatal
  corruption, resource limits), and each binding maps them to the host language's native error types
  — never to strings a caller must parse.
- An unsafe/FFI change needs an explicit invariant review: ownership and lifetimes across the
  boundary, null / UB, foreign-error-to-`Result` translation (the unsafe-ffi reviewer agent).
- A change here requires building the affected binding with its own toolchain plus that review.
