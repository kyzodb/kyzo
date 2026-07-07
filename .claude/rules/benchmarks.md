---
paths:
  - "kyzo-core/benches/**/*.rs"
  - "kyzo-core/examples/bench_tc.rs"
  - "bench-results/**"
  - "scripts/run-bench.sh"
---

# Benchmarks

A benchmark regression is not closed by explanation. Benchmarks are instruments, never gates: measure
before and after, publish the losing runs, fix what the instrument catches.

## A benchmark report requires

- baseline commit + current commit
- exact command + environment
- raw results
- correctness result (the workload's answer is unchanged)
- RSS / memory result
- profile evidence
- root cause
- recovery path, or an explicit ruling

## Forbidden

- "intrinsic cost" without profiling
- accepting a hot-loop regression before checking the representation choice
- claiming success without running the baseline
- hiding losing numbers behind narrative

## When a hot loop regresses, first ask

- is it using the DURABLE representation where the EXECUTION representation is lawful?
- is it re-encoding values already interned?
- is it re-interning values that should flow as codes?
- is it allocating in a path that should be packed/borrowed?

(The measured trap: keying a temp store by codes while re-interning per operation is *slower*, not
faster — the win needs codes to FLOW through the pipeline, interned once. Record such measurements so
a later story cannot repeat the dead end.)

Bench data lives in the sibling `../kyzo-bench` lane; a bench-exposed engine defect becomes an issue
here and is fixed here.
