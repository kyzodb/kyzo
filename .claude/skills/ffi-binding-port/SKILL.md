---
name: ffi-binding-port
description: The checklist for porting one language binding to KyzoDB (stories #5-#10 in-workspace; #11-#14 separate repos). Use when picking up any binding story. Each binding is an unsafe/foreign-toolchain zone not covered by core CI.
---

# FFI binding port

Each binding story (C #5, Python #6, Java #7, Node #8, Swift #9, WASM #10 in-workspace; Go #11,
Clojure #12, Android #13, Python client #14 in separate repos) follows this checklist end to end. A
green core build says **nothing** about a binding; every step below is verified with the binding's own
toolchain.

## Checklist

1. **Depends on product green (#4).** The binding builds against a green, pure-Rust `kyzo-core`.
   Go (#11) additionally needs the C ABI (#5), Clojure (#12) needs Java (#7), the Python client (#14)
   needs Python (#6).
2. **Rework the FFI surface** against the KyzoDB engine API: the binding's FFI layer (`extern "C"`
   + `cbindgen`, `pyo3`, `jni`, `neon`/napi, `swift-bridge`, `wasm-bindgen`) is intrinsic and stays;
   no storage-backend or C/C++ build plumbing comes with it.
3. **Rebrand** `cozo` -> `kyzo`: crate/package name, module/namespace names, published-artifact
   coordinates (PyPI/Maven/npm/etc.), docs. **Preserve every MPL copyright header and all attribution
   verbatim**; add ours alongside, never overwrite.
4. **Build with the binding's own toolchain** (CPython, JVM, Node, Swift, wasm target). Quote the real
   build output; the core CI does not cover this.
5. **Test** with the binding's own test harness, and exercise at least one real query round-trip through
   the FFI boundary.
6. **Unsafe-invariants review is gating**: dispatch the `unsafe-ffi-reviewer` agent on the diff
   (ownership/lifetimes across the boundary, null/UB, foreign-error-to-`Result` translation) and resolve
   or consciously accept every finding.
7. **Draft the publish, do not publish.** Package artifacts and release steps are prepared and shown;
   publishing to PyPI/Maven/npm/etc. waits for an explicit go from the maintainer.

## Anti-avoidance

The bindings are committed work. Never re-frame a binding as "later", "optional", or "out of scope for
pure Rust"; a binding's FFI is what a binding *is*. If a step is hard (e.g. a Swift toolchain on Linux),
name the difficulty plainly and solve or escalate it — do not silently drop the story.
