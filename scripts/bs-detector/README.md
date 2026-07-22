# Bullshit Detector

```
crates/bs-detector/
├── Cargo.toml            workspace member — INSIDE its own boundary, scans itself
├── checks.toml           THE REGISTRY: every check as data — shape, engine, boundary, policy.
│                         Scope lives here and ONLY here.
├── waivers.toml          every sworn testimony in one auditable, diffable file
├── src/
│   ├── main.rs           the one CLI door (run all / --only <check>)
│   ├── boundary.rs       Boundary type + the one tree walk + CoverageProof
│   ├── registry.rs       checks.toml → typed Checks; no BiteProof or unwaivered
│   │                     narrow scope = fails to load
│   ├── policy.rs         Policy = HardBan | SwornWaiver
│   ├── waiver.rs         live-target-bound Waiver; drift = violation
│   ├── verdict.rs        the ONLY writer of the gate log, evidence-bearing
│   ├── report.rs         human + machine output (frozen contract for hooks/CI)
│   ├── engines/          exactly four: shape.rs, graph.rs, behavior.rs, meta.rs
│   └── matchers/         per-LieShape predicates as data, not files-with-loops
└── tests/bite_proofs.rs  every registered check detonates on its historical fixture
```
