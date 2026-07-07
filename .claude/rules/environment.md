---
paths:
  - "Dockerfile"
  - "docker-compose.yml"
  - "justfile"
  - "rust-toolchain.toml"
  - "scripts/**"
  - "bench-results/**"
---

# Gate Environment

Ambiguity about the machine is not allowed in a gate report. The repo pins its own environment; use
it, and say you did.

## The pinned environment

- Toolchain: `rust-toolchain.toml` (channel + components). Dependencies: `Cargo.lock`.
- OS/build box: `Dockerfile` (pure-Rust — gcc as linker only; NO clang/cmake/protobuf/openssl, so a
  C-source dependency fails to build here, one rung above `scripts/check-pure-rust.sh`).
- Named commands: `justfile`. Two services in `docker-compose.yml`: `kyzo-dev` (32 GiB RSS,
  parallel) and `kyzo-bench` (96 GiB, single-threaded).

## The rule

- **EVERY cargo run goes through the container. There is no native path.** Build/test/clippy/bench are
  ALWAYS `docker compose run --rm kyzo-dev just <recipe>` (or `kyzo-bench just bench`). A raw native
  `cargo test`/`cargo build`/`cargo clippy` or a bare native `just <compiling-recipe>` is a defect —
  `pre-bash-guard.sh` blocks it and steers you to the container.
- **Never hand-set a memory or parallelism limit.** No `ulimit -v`, no `timeout`, no
  `--test-threads`. The container's cgroup RSS ceiling (`mem_limit`) and pinned `RUST_TEST_THREADS`
  ARE the limits, prebaked in `docker-compose.yml`. `ulimit -v` caps virtual address space (which Rust
  over-reserves) and manufactures fake OOMs — it is banned.
- **Benchmark reports include:** service (`kyzo-dev`/`kyzo-bench`), CPU count, `memory.max`,
  `RUST_TEST_THREADS`, raw results, correctness result, and peak RSS — all read from inside the
  container (`just env-report`/`just memcheck`). All bench lives in THIS repo (`bench-results/`,
  `examples/bench_tc.rs`, `scripts/run-bench.sh`); there is no external bench lane.

## No mindless ratchet

The gate is **"no weakened test, every gate green, no invariant lost,"** NOT "the number went up." A
test count may fall when a redundant or lower-quality test is replaced by a stronger one, or when a
whole surface is deleted for a better design — that is progress, not regression. Bias for the greatest
engine; never preserve an old test, fixture, or number that no longer serves. What must never fall is
COVERAGE OF A LAW: a deleted test whose law still holds must have a stronger replacement, ledgered.
