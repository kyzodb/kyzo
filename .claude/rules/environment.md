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

- **Official story gates run in the container:** `docker compose run --rm kyzo-dev just gate`. Native
  runs are allowed for speed, but every gate report states **native vs containerized**.
- **The memory cap is the container's cgroup RSS ceiling, never `ulimit -v`.** `ulimit -v` caps
  virtual address space, which Rust's per-thread allocator arenas over-reserve; it manufactures fake
  OOMs at high parallelism while real RSS is fine. A gate report proving an OOM must show
  `/sys/fs/cgroup/memory.max`, `RUST_TEST_THREADS`, peak RSS (`/usr/bin/time -v` / `VmHWM`), and
  whether it was a kernel OOM-kill, a Rust allocation failure, or a timeout.
- **Benchmark reports include:** environment (native/container + service), CPU count, memory limit,
  `RUST_TEST_THREADS`, raw results, correctness result, and peak RSS. All bench lives in THIS repo
  (`bench-results/`, `examples/bench_tc.rs`, `scripts/run-bench.sh`); there is no external bench lane.

## No mindless ratchet

The gate is **"no weakened test, every gate green, no invariant lost,"** NOT "the number went up." A
test count may fall when a redundant or lower-quality test is replaced by a stronger one, or when a
whole surface is deleted for a better design — that is progress, not regression. Bias for the greatest
engine; never preserve an old test, fixture, or number that no longer serves. What must never fall is
COVERAGE OF A LAW: a deleted test whose law still holds must have a stronger replacement, ledgered.
