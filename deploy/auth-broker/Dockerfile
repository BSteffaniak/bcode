# syntax=docker/dockerfile:1.7
# Build context is the repository root. No release artifact or credentials required.
FROM rust:1.95-bookworm AS build
ENV RUSTUP_TOOLCHAIN=1.95.0
WORKDIR /src
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates cmake pkg-config clang libssl-dev \
    && rm -rf /var/lib/apt/lists/*
COPY Cargo.toml Cargo.lock rust-toolchain.toml clippy.toml ./
COPY .cargo .cargo
COPY packages packages
COPY plugins plugins
COPY catalog catalog
COPY workers workers
COPY xtask xtask
COPY examples examples
# Keep this feature set aligned with the source-built GitHub Action.
RUN cargo build --locked --release -p bcode --features app,static-bundled-plugins

FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates libssl3 bash tini util-linux coreutils grep \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 broker \
    && useradd --uid 10001 --gid 10001 --no-create-home --home-dir /data broker \
    && mkdir -p /opt/broker /run/bcode /data
COPY --from=build /src/target/release/bcode /usr/local/bin/bcode
COPY deploy/auth-broker/supervise.sh /opt/broker/supervise.sh
ENV HOME=/data BCODE_CONFIG=/data/broker.toml RUST_LOG=off
WORKDIR /opt/broker
EXPOSE 8080
# Root only stages the owner-only identity and volume directories. Children run as 10001.
ENTRYPOINT ["/usr/bin/tini", "--", "/bin/bash", "/opt/broker/supervise.sh"]
CMD ["serve"]
