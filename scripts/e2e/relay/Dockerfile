# wait-agent relay e2e image (issue #37, slice 1): builder compiles the
# release binary inside docker so the harness works on any host with docker;
# the runtime stage carries only the binary (ldd: libc/libgcc/libm).
#
# The runtime base is ubuntu:24.04 (glibc) rather than debian:bookworm-slim:
# both satisfy the binary's three shared libs, and ubuntu:24.04 is far more
# likely to be present on air-gapped/mirror-only hosts (this repo's CI and
# dev machines both run Ubuntu 24.04, whose glibc 2.39 also covers binaries
# linked on bookworm's older glibc).
FROM rust:1.88-bookworm AS builder

# System deps mirror the CI build job (.github/workflows/ci.yaml).
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        bison pkg-config libevent-dev libncurses-dev \
        build-essential automake autoconf \
        libdbus-1-dev libkeyutils-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY . .
RUN cargo build --release

FROM ubuntu:24.04
COPY --from=builder /src/target/release/waitagent /usr/local/bin/waitagent
