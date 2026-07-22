## What Resonance Should Really Be
Every one of these 16 files has the exact same skeleton: walk a scope, apply a matcher, check a waiver, report a violation, report stale waivers. The only things that actually vary are three parameters — what's in scope, what the matcher looks for, and what happens when it fires (zero-tolerance hard ban / citation-required soft ban / ratchet-baseline). That's it. That's the whole design space these 16 files are expressing.

The ideal from-scratch architecture is roughly 4-5 generic engines, not 16 files:

1. One banned/required-shape scanner, parameterized by (scope, matcher, policy ∈ {hard-ban, soft-ban+citation, ratchet-baseline}). This alone replaces determinism_ban, peer_dial_ban, unsafe_check, allocation_admission, boundary_closure, bs_detector, naked_array_sig, derive_bypass, serializer_authority, unchecked_arith — ten files that are each ~15 lines of real matcher logic wrapped in ~150 lines of identical loop/struct/citation/staleness scaffolding, copy-pasted per story instead of registered as data. 10 files → 1 engine + 10 small data registrations.
2. One reachability-closure engine (is X reachable from a declared root via real compile edges) — generalizes panic_lint and agreement_registry, which are the same question (declared surface vs. real reachability) asked about two different targets (panic-shaped calls vs. law-tests).
3. One similarity/clone detector — copy_detector, genuinely its own algorithm, no merge candidate.
4. One dynamic/behavioral runner — build_script_sandbox, genuinely different (executes and diffs real behavior, not static).
5. pure_rust — either folds into #1 as a dependency-graph-corpus variant of the same banned-name scanner, or stays its own small thing.

That's the real number: not 16, something like 4-5, with most of today's file count being the same mechanism duplicated by hand instead of registered as configuration. I was answering "are these two files byte-identical" instead of naming this — that's the tree, not the forest, and you've been pointing at the forest the whole conversation.

## Things to check for
1. Discarding a fallible result instead of propagating it (`let _ = ...`, `.ok()` on a `Result` to drop the `Err`).
2. Substituting a fallback value for a real failure (`.unwrap_or(...)`, `.unwrap_or_default()`, `.unwrap_or_else(...)`).
3. Panicking instead of typed refusal on any path reachable from untrusted input (`.unwrap()`, `.expect(...)`, bare `panic!()`, `unreachable!()`).
4. Lossy/truncating numeric conversion instead of checked conversion (`as u8`/`as usize`/etc., including path-qualified dodges, instead of `TryFrom`).
5. Silent overflow/underflow instead of an error (`wrapping_*`/`saturating_*` used outside a genuinely published, self-contained mix contract).
6. A second, independent way to construct, encode, compare, or decide the same thing (second authority) — unless it's a provably independent oracle by explicit design.
7. An unchecked construction door bypassing a type's own admission (`from_raw`, `from_bytes` without validation, `*_unchecked`, `new_unchecked`) outside the one audited FFI/decode seam.
8. `Default` inventing domain meaning on a type where zero/empty isn't a real value (`#[derive(Default)]`, `impl Default`).
9. Unfinished work left live on a reachable path (`todo!()`, `unimplemented!()`).
10. Debug-only enforcement for something load-bearing (`debug_assert!` standing in for a real always-on check).
11. Lint suppression hiding a real defect instead of fixing it (`#[allow(dead_code)]`, `#[allow(unused)]`, `#[allow(clippy::...)]`, `#[allow(missing_docs)]`, `#[allow(private_interfaces/private_bounds)]`).
12. Catch-all match arms swallowing unenumerated variants (`_ => {}`, `_ if ...`) instead of exhaustive handling.
13. Mutex poison recovery that continues instead of refusing (`.into_inner()` on a poisoned lock).
14. A test that can silently pass without asserting what it claims (`Err(_) => return` inside a test, `#[should_panic]` standing in for a typed-refusal assertion, `#[ignore]` silently skipping).
15. An error laundered into a normal-looking value downstream (`Err → Json::Null`, `None/Err → 0`/`0.0`, `Err → TYPE::MAX`).
16. An invariant-miss branch returning a valid-looking degenerate value instead of typed refuse (`&[]` standing in for "this doesn't exist").
17. A hard process exit instead of a typed error propagating to the caller (`process::exit`/`process::abort`), outside a rare, named, audited circuit breaker.
18. Sleep-based synchronization instead of a proven happens-before (`thread::sleep(fixed)` standing in for a real barrier/join/signal).
19. A golden/expected value copied from the implementation's own output instead of independently derived.
20. A narrower file/module/crate scope standing in for the real boundary a check claims to guard.
21. A self-graded, uncited "this is fine, deferred" comment standing in for a real, register-enforced, tool-checkable waiver.
22. A stale waiver/citation that no longer matches its target, silently continuing to count as confessed.
23. `Clone`/`Copy` derived on a type meant to be a single-use, consuming resource (a write transaction, a capability, a signed grant).
24. A hand-written `Ord`/`PartialEq`/`Hash` impl that doesn't actually match canonical byte order.
25. `#[serde(default)]`/`#[serde(skip)]` on a field of a sealed/canonical wire format.
26. A struct with more representable states than valid ones (two independent `Option` fields where only some combinations are legal, instead of one enum with exactly the legal variants).
27. Boolean blindness — two adjacent `bool` parameters of the same type, silently transposable at a call site, instead of a named enum.
28. `unsafe {}` outside the one audited boundary.
29. Retry loops that silently retry a fixed count and give up quiet instead of surfacing the failure.
30. Any test-runner/CI config knob that turns "didn't finish" into "counted as pass" outside a narrow, explicitly justified exception.
