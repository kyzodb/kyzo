# Pinned gate/dev environment for KyzoDB.
#
# The engine is PURE RUST. This image installs gcc ONLY as the linker driver
# (Rust links through `cc` even for pure-Rust binaries); it deliberately does
# NOT install any C-SOURCE build tooling — no clang, cmake, protobuf, or
# openssl-dev. A dependency that tries to compile C therefore fails to build
# in the gate container, machine-enforcing the pure-Rust invariant one rung
# above scripts/check-pure-rust.sh.
#
# The exact toolchain is pinned by rust-toolchain.toml (1.96.1); rustup honors
# it on the first cargo invocation regardless of the base tag.
FROM rust:1.96.1-bookworm

RUN apt-get update && apt-get install -y --no-install-recommends \
      git \
      jq \
      time \
      ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# `just` is the command runner (pure Rust; not in Debian's default apt).
# Compiled once into the image layer.
RUN cargo install just --locked

# Caches and artifacts live OUTSIDE the bind-mounted repo (named volumes in
# compose), so container builds never contaminate the host's native target/
# and vice versa.
ENV CARGO_HOME=/cargo \
    CARGO_TARGET_DIR=/target \
    RUST_BACKTRACE=1

WORKDIR /workspace

# Materialize the pinned toolchain into the image layer.
COPY rust-toolchain.toml ./
RUN rustup show && rustc --version

CMD ["bash"]
