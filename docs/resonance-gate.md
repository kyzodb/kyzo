# The Resonance Gate

Static law enforcement for the ruled architecture. The gate scans every
source file under `crates/` and mechanically verifies that the code's
*shape* still obeys the seats in `docs/decisions.md`. It runs in seconds, on
every file change, and its verdict blocks work from silently drifting off
the law. It proves **shape**; tests and DST campaigns prove **behavior**.

Why it exists: prose review and agent testimony certified a violated seat
for days (the seat-59 second-serializer disaster), and a text-grep "proof"
kept 6000 lines of dead law tests green (the `rehomed_from_core` disaster).
Any seat violation detectable from source shape alone must turn something
red automatically. Testimony is never the meter.

The gate is the **bs-detector** crate. The old xtask `resonance` verb, its
per-check source files, the root allowlist, and every ratchet baseline were
deleted in the consolidation cutover — baselines do not exist; the baseline
is zero, forever.

## Where things live

| Piece | Path |
| --- | --- |
| Check registry (checks as data) | `crates/bs-detector/checks.toml` |
| Engines (shape / graph / meta) | `crates/bs-detector/src/engines/*.rs` |
| Sworn waivers + scope waivers | `crates/bs-detector/waivers.toml` |
| Bite proofs (one per check) | `crates/bs-detector/tests/bite_proofs.rs` |
| Agreement-law registry | `crates/xtask/agreements.toml` |
| File watcher | `crates/xtask/resonance-watch.sh` |
| Combined fast gate | `crates/xtask/gate-fast.sh` |
| Live verdict log | `crates/xtask/resonance.log` (line 1 = verdict) |
| Counts artifact | `crates/xtask/bs-counts.txt` |
| Stop hook / watcher hooks | `.claude/hooks/resonance-stop-guard.sh`, `.claude/hooks/ensure-resonance-watch.sh` |

## Usage

The one door (host or kyzo-dev container):

```
cargo run --release -p bs-detector -- --root .          # full run, writes artifacts
cargo run --release -p bs-detector -- --root . --only unwrap   # focused; never writes
cargo test -p bs-detector                               # bite-proof suite
```

`cargo xtask gate` runs the detector as its conduct step; CI's
`resonance-gate` job runs the dry-run plus the bite-proof suite.

Architecture and file tree: `scripts/bs-detector/README.md`. The banned-shape
taxonomy: `BANNED.md`.
