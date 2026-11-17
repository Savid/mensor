# mensor, built from this repo. The image's toolchain, not the one
# rust-toolchain.toml names with clippy, rustfmt and wasm32 beside it.
FROM rust:1.97.1-slim-trixie AS build
ENV RUSTUP_TOOLCHAIN=1.97.1
# `postgres` to reach a Postgres store, as compose.postgres.yaml asks.
ARG FEATURES=""
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p mensor --bin mensor --features "$FEATURES"

# The host and the hands share this image. The tools are for the model's
# commands: add what your work needs.
FROM debian:trixie-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl git jq python3 ripgrep \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/mensor /usr/local/bin/mensor
RUN useradd --create-home mensor \
    && mkdir /data /home/mensor/work \
    && chown mensor /data /home/mensor/work
USER mensor
WORKDIR /home/mensor
# mensor is PID 1, with no init: an init would hold the environment, the
# token in it, where the model's commands could read it.
ENTRYPOINT ["mensor"]
