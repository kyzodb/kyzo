---
name: ffi-binding
description: The checklist for building one language binding for KyzoDB (stories #5-#10 in-workspace; #11-#14 separate repos, milestoned Version 1.0.0 / Future Roadmap). Use when picking up any binding story. Each binding is an unsafe/foreign-toolchain zone not covered by core CI.
---

# FFI binding

Each binding story (C #5, Python #6, Java #7, Node #8, Swift #9, WASM #10 in-workspace; Go #11,
Clojure #12, Android #13, Python client #14 in separate repos) follows this checklist end to end. A
green core build says **nothing** about a binding; every step below is verified with the binding's
own toolchain.

## Checklist

1. **Builds against green main.** The binding builds against the current, pure-Rust `kyzo-core` on
   main — no pinned engine snapshots, no compatibility shims for a stale API. Go (#11) additionally
   needs the C ABI (#5), Clojure (#12) needs Java (#7), the Python client (#14) needs Python (#6).
2. **Design the surface for OUR engine.** The FFI layer (`extern "C"` + `cbindgen`, `pyo3`, `jni`,
   `neon`/napi, `swift-bridge`, `wasm-bindgen`) exposes the KyzoDB engine API — typed errors, time
   travel, the value plane — not a translation of what some other wrapper once exposed. Cozo's
   binding code targets a dead API: a reference for FFI mechanics at most, never a design
   authority; no storage-backend or C/C++ build plumbing comes with it.
3. **Names are ours; attribution is preserved.** Crate/package/module/namespace names and
   published-artifact coordinates (PyPI/Maven/npm/etc.) are KyzoDB's own. Where code is derived
   from cozo's bindings, **preserve every MPL copyright header and all attribution verbatim**; add
   ours alongside, never overwrite.
4. **Build with the binding's own toolchain** (CPython, JVM, Node, Swift, wasm target). Quote the real
   build output; the core CI does not cover this.
5. **Translate typed errors, never strings**: the kernel's typed errors (`ConflictError` = retryable,
   corruption = fatal, limit-exceeded = resource) map to the language's native exception/error types so
   host code can branch on them; string-matching across an FFI boundary is a defect.
6. **Test** with the binding's own test harness, and exercise at least one real query round-trip through
   the FFI boundary.
7. **Unsafe-invariants review is gating**: dispatch the `unsafe-ffi-reviewer` agent on the diff
   (ownership/lifetimes across the boundary, null/UB, foreign-error-to-`Result` translation) and resolve
   or consciously accept every finding.
8. **Draft the publish, do not publish.** Package artifacts and release steps are prepared and shown;
   publishing to PyPI/Maven/npm/etc. waits for an explicit go from the maintainer.

## Anti-avoidance

The bindings are committed work. Never re-frame a binding as "later", "optional", or "out of scope for
pure Rust"; a binding's FFI is what a binding *is*. If a step is hard (e.g. a Swift toolchain on Linux),
name the difficulty plainly and solve or escalate it — do not silently drop the story.
