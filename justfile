# KyzoDB gates, made boring. Run natively for speed, or in the pinned
# container for an official gate:
#
#     docker compose run --rm kyzo-dev  just gate      # the seal
#     docker compose run --rm kyzo-bench just bench     # measurements
#
# No `ulimit -v` here: the container's cgroup RSS limit (compose mem_limit) is
# the real, honest cap — `ulimit -v` caps VIRTUAL address space, which Rust's
# per-thread allocator arenas over-reserve, and that was the sole source of the
# "12 GB cap / OOM at -j4 / passes single-threaded" noise. A real RSS ceiling
# ends it.

set shell := ["bash", "-euo", "pipefail", "-c"]

# The one-command seal: everything that must be true to close a story.
gate: env-report fetch check fmt clippy unsafe pure-rust test test-features
    @echo "=== GATE PASSED ==="

# Warm the dep cache deterministically so tree/metadata-parsing gates
# (pure-rust) don't see cold-cache fetch noise.
fetch:
    cargo fetch --locked

# Environment fingerprint — the boring, unarguable answer.
env-report:
    @echo "container memory.max: $(cat /sys/fs/cgroup/memory.max 2>/dev/null || echo 'native (no cgroup limit)')"
    @echo "RUST_TEST_THREADS:    ${RUST_TEST_THREADS:-unset (cargo default)}"
    @echo "nproc:                $(nproc)"
    @echo "toolchain:            $(rustc --version)"

check:
    cargo check --workspace --all-targets

# Default config, lib + integration.
test:
    cargo test -p kyzo --release

# Features config (bench/fuzz internals).
test-features:
    cargo test -p kyzo --release --features bench-internals,fuzz-internals --lib

fmt:
    cargo fmt --check -p kyzo

# Own-code -D warnings, both feature configs. `--no-deps` excludes the vendored
# lsm-tree/fjall path deps (their clippy state is #118's, not a story gate).
clippy:
    cargo clippy -p kyzo --release --all-targets --no-deps -- -D warnings
    cargo clippy -p kyzo --release --all-targets --no-deps --features bench-internals,fuzz-internals -- -D warnings

unsafe:
    bash scripts/check-unsafe.sh

pure-rust:
    bash scripts/check-pure-rust.sh

# The seal with peak RSS attached — proves the memory envelope, no vibes.
memcheck:
    /usr/bin/time -v cargo test -p kyzo --release 2>&1 | grep -E "Maximum resident set size|Elapsed \(wall"

# Transitive-closure benchmark over the SNAP graphs (run in kyzo-bench).
bench *graphs:
    bash scripts/run-bench.sh {{graphs}}
