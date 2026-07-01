---
paths:
  - "**/Cargo.toml"
---
# Rule: Cargo features & dependencies

KyzoDB's default build is **`fjall`, the pure-Rust LSM KV backend**. There are no storage-backend
feature flags and no C/C++ dependency in `kyzo-core` or `kyzo-bin`.

- `kyzo-core` and `kyzo-bin` must stay **pure Rust**: a dependency or feature that pulls a C or C++
  compiler is a regression of the whole point of KyzoDB.
- The language bindings each carry their own FFI dependency (`pyo3`, `jni`, `neon`, `swift-bridge`,
  `wasm-bindgen`, the C ABI). That is intrinsic to a binding and separate from the engine.

**A change here requires:** confirming the default (pure-Rust) build still compiles and that no C/C++
dependency is introduced into the engine.
