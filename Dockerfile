# mensor's image: the host and the hands share it. Built from the repo or
# for a release, it holds a static (musl) mensor built in the same rust
# image, by digest: the build stage makes one for the architecture it runs
# on, as CI's binaries job does for amd64 and arm64. A release passes CI's
# in with `--build-context binaries=<dir>`, mensor at <dir>/<arch>/mensor,
# which replaces the stage of that name, so the build stage is skipped.

# The toolchain rust-toolchain.toml pins, without the clippy, rustfmt and
# wasm32 it adds.
FROM rust:1.97.1-slim-trixie@sha256:8e8cf8f7fd54a2d23d5a743b3a03f56e26b6c774276c33fa0595111704ebb15c AS build
# musl-tools has musl-gcc on every architecture, but a compiler named for
# the target only on some: name it, for ring's C and SQLite's.
ENV RUSTUP_TOOLCHAIN=1.97.1 TARGET_CC=musl-gcc
RUN apt-get update \
    && apt-get install -y --no-install-recommends musl-tools \
    && rm -rf /var/lib/apt/lists/* \
    && rustup target add "$(uname -m)-unknown-linux-musl"
# `postgres` to reach a Postgres store; empty for a binary without a
# Postgres client.
ARG FEATURES=postgres
ARG TARGETARCH
WORKDIR /src
COPY . .
RUN target="$(uname -m)-unknown-linux-musl" \
    && cargo build --release --locked -p mensor --bin mensor --features "$FEATURES" --target "$target" \
    && mkdir -p "/binaries/$TARGETARCH" \
    && cp "target/$target/release/mensor" "/binaries/$TARGETARCH/"

FROM scratch AS binaries
COPY --from=build /binaries /

# The tools are for the model's commands: add what your work needs.
FROM debian:trixie-20261027-slim@sha256:f6b08b09a05725f9431502d3b5714f5c4576e82c1a84fa1debe0c24a7e0c3ec4
ARG TARGETARCH
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl git jq python3 ripgrep \
    && rm -rf /var/lib/apt/lists/*
COPY --from=binaries /${TARGETARCH}/mensor /usr/local/bin/mensor
RUN useradd --create-home mensor \
    && mkdir /data /home/mensor/work \
    && chown mensor /data /home/mensor/work
USER mensor
WORKDIR /home/mensor
# mensor is PID 1, with no init: an init would hold the environment, the
# token in it, where the model's commands could read it.
ENTRYPOINT ["mensor"]
