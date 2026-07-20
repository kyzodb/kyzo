# bench/results

Benchmark history as plain, committed files — one per run, named by the commit
it was measured at. **A "baseline" is just a file in here.** Compare two runs by
diffing them.

## Standard workloads only
We do not invent data or queries. The benchmark is **transitive closure — the
canonical recursive-Datalog workload — over real published SNAP graphs**
(Stanford Network Analysis Project, `snap.stanford.edu`), the edge lists the
community actually benchmarks on. The graph is a downloaded file; the program is
the textbook two-rule TC. This is the vanilla, community-standard measure for a
Datalog engine; nothing here is bespoke.

## How it works
- `cargo xtask fetch-bench-data` downloads the standard SNAP graphs into
  `bench/data/` per `bench/manifest.json` (URL + SHA-256 per graph). Tampered
  bytes refuse — integrity is computed SHA-256 compare, not filename/length.
- `scripts/run-bench.sh` builds `examples/bench_tc.rs`, runs it over each graph
  at `HEAD`, and writes `bench/results/<short-sha>.txt` stamped with commit,
  date, and machine.
- Each line: `TC graph=<> edges=<> nodes=<> variant=count load_ms=<> query_ms=<>
  closure_rows=<> peak_rss_kb=<>`.

## Compare
    scripts/run-bench.sh
    diff bench/results/<older-sha>.txt bench/results/<newer-sha>.txt

Only compare runs from the same machine (each file's header records it).

## History
One file per story-wave lands here as the engine improves — the diff between
consecutive files is how we show progress.
