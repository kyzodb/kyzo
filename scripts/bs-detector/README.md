# Bullshit Detector
Every one of these 16 files has the exact same skeleton: walk a scope, apply a matcher, check a waiver, report a violation, report stale waivers. The only things that actually vary are three parameters — what's in scope, what the matcher looks for, and what happens when it fires (zero-tolerance hard ban / citation-required soft ban / ratchet-baseline). That's it. That's the whole design space these 16 files are expressing.

The ideal from-scratch architecture is roughly 4-5 generic engines, not 16 files:

1. One banned/required-shape scanner, parameterized by (scope, matcher, policy ∈ {hard-ban, soft-ban+citation, ratchet-baseline}). This alone replaces determinism_ban, peer_dial_ban, unsafe_check, allocation_admission, boundary_closure, bs_detector, naked_array_sig, derive_bypass, serializer_authority, unchecked_arith — ten files that are each ~15 lines of real matcher logic wrapped in ~150 lines of identical loop/struct/citation/staleness scaffolding, copy-pasted per story instead of registered as data. 10 files → 1 engine + 10 small data registrations.
2. One reachability-closure engine (is X reachable from a declared root via real compile edges) — generalizes panic_lint and agreement_registry, which are the same question (declared surface vs. real reachability) asked about two different targets (panic-shaped calls vs. law-tests).
3. One similarity/clone detector — copy_detector, genuinely its own algorithm, no merge candidate.
4. One dynamic/behavioral runner — build_script_sandbox, genuinely different (executes and diffs real behavior, not static).
5. pure_rust — either folds into #1 as a dependency-graph-corpus variant of the same banned-name scanner, or stays its own small thing.

That's the real number: not 16, something like 4-5, with most of today's file count being the same mechanism duplicated by hand instead of registered as configuration. I was answering "are these two files byte-identical" instead of naming this — that's the tree, not the forest, and you've been pointing at the forest the whole conversation.

## Target layout (design only — not built yet)

```
scripts/bs-detector/
├── README.md            this doc
├── Cargo.toml           standalone binary crate — own deps, no dependency on crates/xtask
├── checks.toml           DATA: one entry per registered check — name, scope glob, matcher ref, policy.
│                          Replaces the 10 hand-written hard/soft-ban files' worth of consts.
├── src/
│   ├── main.rs            CLI entrypoint: point it at any directory, load checks.toml, run, report, exit code.
│   ├── scope.rs           generic directory walk + scope predicate ("what's in scope") — the one shared walker.
│   ├── waiver.rs           citation/allowlist loading + hard-ban/soft-ban/ratchet policy enforcement,
│   │                        including stale-waiver detection. One implementation, not one per check.
│   ├── report.rs          violation formatting + PASS/FAIL machine-surface output (frozen contract).
│   ├── engines/
│   │   ├── mod.rs          the Engine trait (run(scope, files) -> Vec<Violation>) + registry.
│   │   ├── shape_scanner.rs  Engine 1 — generic banned/required-shape scanner, parameterized by
│   │   │                      (scope, matcher, policy). Replaces determinism_ban, peer_dial_ban,
│   │   │                      unsafe_check, allocation_admission, boundary_closure, bs_detector,
│   │   │                      naked_array_sig, derive_bypass, serializer_authority, unchecked_arith.
│   │   ├── reachability.rs   Engine 2 — is X reachable from a declared root via real compile edges.
│   │   │                      Replaces panic_lint and agreement_registry (same question, two targets).
│   │   ├── similarity.rs     Engine 3 — token-shingle near-duplicate detector. Replaces copy_detector.
│   │   │                      No merge candidate; genuinely its own algorithm.
│   │   ├── sandbox.rs        Engine 4 — dynamic behavioral runner (executes + diffs real behavior).
│   │   │                      Replaces build_script_sandbox. Not static analysis — its own class.
│   │   └── dep_scan.rs       Engine 5 — banned-name scan over a dependency graph, not source files.
│   │                          Replaces pure_rust; open choice whether this folds into shape_scanner
│   │                          as a corpus adapter or stays separate.
│   └── matchers/
│       └── mod.rs           the ~10 small matcher predicates that turn Engine 1 into each original
│                              check — each is data (a closure/fn), not a file with its own loop,
│                              struct, citation validation, and staleness logic.
└── tests/
    └── bite_proofs.rs      one bite-proof per registered check/engine, proving it demonstrably goes
                              red on its historical bug shape — same law as today's bite-proof script.
```

Nothing above exists yet — this is the target shape the conversation converged on, recorded so the next build starts from it instead of re-deriving it.