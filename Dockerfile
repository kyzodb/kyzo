# Pinned gate/dev environment for KyzoDB.
#
# The engine is PURE RUST. This image installs gcc ONLY as the linker driver
# (Rust links through `cc` even for pure-Rust binaries); it deliberately does
# NOT install any C-SOURCE build tooling — no clang, cmake, protobuf, or
# openssl-dev. A dependency that tries to compile C therefore fails to build
# in the gate container, machine-enforcing the pure-Rust invariant one rung
# above the xtask `pure-rust` verb (crates/xtask/src/checks/pure_rust.rs).
#
# The exact toolchain is pinned by rust-toolchain.toml (1.96.1); rustup honors
# it on the first cargo invocation regardless of the base tag.
#
# Runtime user matches the host developer (build-args USER_UID / USER_GID,
# default 1000). Running as root on a bind-mounted repo is how host ./target
# got root-owned; that path is closed.
FROM rust:1.96.1-bookworm@sha256:a339861ae23e9abb272cea45dfafde21760d2ce6577a70f8a926153677902663

RUN apt-get update && apt-get install -y --no-install-recommends \
      git \
      jq \
      rsync \
      time \
      curl \
      ca-certificates \
      fuse3 \
    && rm -rf /var/lib/apt/lists/*

# Caches and artifacts live OUTSIDE the bind-mounted repo (named volumes in
# compose), so container builds never contaminate the host's native target/
# and vice versa. Keep PATH/RUSTUP_HOME explicit — login shells otherwise drop
# /usr/local/cargo/bin from the official rust image ENV.
ENV PATH=/usr/local/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin \
    RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/cargo \
    CARGO_TARGET_DIR=/target \
    RUST_BACKTRACE=1

WORKDIR /workspace

# Materialize the pinned toolchain into the image layer (as root; toolchain
# lives under the image's rustup paths, not the runtime /cargo volume).
COPY rust-toolchain.toml ./
RUN rustup show && rustc --version


# cargo-nextest is the ONLY test runner in this project — plain `cargo test`
# is banned (pre-bash-guard.sh nudges against it; every Check/CI invocation
# uses `cargo nextest run`). Per-test process isolation gives every test a
# real slow-timeout/terminate-after (.config/nextest.toml); a hung test now
# produces a killed-process report instead of a wedged container.
#
# Pinned prebuilt binary, checksum-verified, installed to /usr/local/bin —
# deliberately NOT `cargo install` and NOT $CARGO_HOME/bin:
#   - `cargo install cargo-nextest` compiles nextest's own dependency tree,
#     which needs real C build tooling; this image intentionally carries
#     none (see the pure-Rust note above) and a source build would also cost
#     minutes on every image rebuild for no reproducibility gain.
#   - $CARGO_HOME (/cargo) is a named volume, mounted at container-RUN time,
#     not part of this image. Docker only copies an image's content into a
#     named volume the first time that volume is empty — a cargo-cache
#     volume that already holds downloaded crates is never empty, so
#     anything this Dockerfile put under /cargo would be silently shadowed
#     by the volume forever, surviving no image rebuild. /usr/local/bin has
#     no volume mounted over it, so the pinned binary here is guaranteed
#     live on every container, every rebuild.
ARG NEXTEST_VERSION=0.9.140
RUN set -eux; \
    arch="$(dpkg --print-architecture)"; \
    case "$arch" in \
      amd64) target=x86_64-unknown-linux-gnu;  sha256=4ee9aaa0d0171a985a5d0eb735b87355894c1c455972e9674fb9fdbd1387c9a3 ;; \
      arm64) target=aarch64-unknown-linux-gnu; sha256=8b3f4d4560b6b0f83774fecc6be07e47716dbad0eb0bb6c3890f478f4affe4b6 ;; \
      *) echo "unsupported architecture for pinned cargo-nextest: $arch" >&2; exit 1 ;; \
    esac; \
    url="https://github.com/nextest-rs/nextest/releases/download/cargo-nextest-${NEXTEST_VERSION}/cargo-nextest-${NEXTEST_VERSION}-${target}.tar.gz"; \
    curl -sSfLo /tmp/cargo-nextest.tar.gz "$url"; \
    echo "${sha256}  /tmp/cargo-nextest.tar.gz" | sha256sum -c -; \
    tar -xzf /tmp/cargo-nextest.tar.gz -C /usr/local/bin cargo-nextest; \
    rm /tmp/cargo-nextest.tar.gz; \
    cargo-nextest --version

# Match the host developer. Compose passes USER_UID/USER_GID from the host
# (`id -u` / `id -g`). Named volumes /cargo and /target are created empty and
# owned here so the first container write is not root.
ARG USER_UID=1000
ARG USER_GID=1000
RUN set -eux; \
    groupadd --gid "${USER_GID}" kyzo; \
    useradd --uid "${USER_UID}" --gid "${USER_GID}" --create-home --shell /bin/bash kyzo; \
    mkdir -p /cargo /target; \
    chown -R kyzo:kyzo /cargo /target /home/kyzo; \
    printf '%s\n' \
      'export PATH=/usr/local/cargo/bin:$PATH' \
      'export RUSTUP_HOME=/usr/local/rustup' \
      'export CARGO_HOME=/cargo' \
      'export CARGO_TARGET_DIR=/target' \
      > /etc/profile.d/rust-kyzo.sh; \
    printf '%s\n' \
      'export PATH=/usr/local/cargo/bin:$PATH' \
      'export RUSTUP_HOME=/usr/local/rustup' \
      'export CARGO_HOME=/cargo' \
      'export CARGO_TARGET_DIR=/target' \
      >> /home/kyzo/.bashrc; \
    chown kyzo:kyzo /home/kyzo/.bashrc

USER kyzo


CMD ["bash"]
