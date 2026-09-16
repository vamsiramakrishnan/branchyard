FROM rust:1.90.0-bookworm AS build
RUN apt-get update && apt-get install -y --no-install-recommends cmake && rm -rf /var/lib/apt/lists/*
WORKDIR /source
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates
# Test fixtures are compile-time inputs only; they do not enter the final image.
COPY examples ./examples
RUN cargo build --release --locked -p branchyard-server

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=build /source/target/release/branchyard-server /usr/local/bin/branchyard-server
USER 65532:65532
ENTRYPOINT ["branchyard-server"]
