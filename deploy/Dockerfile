# Builds and runs `by serve` (the branchyard-server admission API, in-process
# local-mode engine). See docs/deploy.md.
#
# Default provider is local process execution: **no isolation** beyond the
# container's own user. Harnesses run as this image's non-root user, with
# whatever the image installs on PATH; add a harness's own package layer if
# a served repository needs one at task time.
FROM rust:1.90.0-bookworm AS build
WORKDIR /source
COPY rust-toolchain.toml Cargo.toml Cargo.lock ./
COPY crates ./crates
# `by init plugin` embeds the shipped skills (crates/branchyard-setup/src/skills.rs).
COPY plugins/branchyard/skills ./plugins/branchyard/skills
# --features postgres: deploy/compose.yaml passes `by serve --database
# postgres://...`, which needs this build-time feature (docs/server.md).
RUN cargo build --release --locked --features postgres -p branchyard-cli

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl git \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --create-home --home-dir /home/branchyard --shell /usr/sbin/nologin branchyard
COPY --from=build /source/target/release/by /usr/local/bin/by
USER branchyard
WORKDIR /home/branchyard
HEALTHCHECK --interval=10s --timeout=3s --start-period=5s --retries=3 \
    CMD curl -fsS http://127.0.0.1:8421/healthz || exit 1
ENTRYPOINT ["by", "serve"]
