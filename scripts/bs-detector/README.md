# Bullshit Detector

```
crates/bs-detector/
├── Cargo.toml            workspace member — INSIDE its own boundary, scans itself
├── checks.toml           THE REGISTRY: every check as data — name, engine, policy,
│                         law sentence, bite_proof binding. No scope field exists.
├── waivers.toml          every sworn testimony in one auditable, diffable file:
│                         [[waiver]] = one site-bound confession; [[scope_waiver]] =
│                         the ONLY lawful narrowing, printed on every run
├── src/
│   ├── main.rs           the one CLI door (run all / --only <check>; --only never
│   │                     writes the verdict artifacts — partial scope never speaks
│   │                     for the tree)
│   ├── lib.rs            library surface; the bite-proof suite imports through it
│   ├── run.rs            one gate run: walk → checks → waiver filter → Verdict
│   │                     (the only source of resonance.log / bs-counts.txt)
│   ├── boundary.rs       Boundary: the one tree walk (all of crates/, only target/
│   │                     excluded) + CoverageProof + UnparsedFile reporting
│   ├── registry.rs       checks.toml → typed Checks; refuses dup names, thin laws,
│   │                     unknown matchers, waivers against hard-bans, and any
│   │                     bite_proof not defined as a #[test] fn (parsed, not grepped)
│   ├── policy.rs         Policy = HardBan | SwornWaiver (closed; no ratchet variant
│   │                     is constructible)
│   ├── waiver.rs         site-bound Waiver + ScopeWaiver; drift = stale = red;
│   │                     one waiver confesses exactly one hit
│   └── engines/
│       ├── mod.rs        Hit — the one finding shape
│       ├── shape.rs      site scans: 40 named matchers in one MATCHERS table;
│       │                 ScanClass::Everything vs ProductionOnly (cfg must IMPLY
│       │                 test to exempt; a module merely named tests is production)
│       ├── graph.rs      relational lies: derive_bypass, copy_detector (exact
│       │                 Jaccard via inverted shingle index), agreement_registry
│       └── meta.rs       the detector policing itself: coverage proof, unparsed
│                         files, stale waivers, forbid roots
└── tests/bite_proofs.rs  every registered check detonates on its historical fixture;
                          the registry refuses a check whose proof fn is missing
```

The door: `cargo run --release -p bs-detector -- --root .` (host or kyzo-dev
container). Verdict contract: line 1 of `crates/xtask/resonance.log` is
`RESONANCE: PASS` or `RESONANCE: FAIL <checks>`; `crates/xtask/bs-counts.txt`
is one `name:N … = TOTAL unconfessed` line. Baseline is zero, forever.
