FROM lukemathwalker/cargo-chef:latest-rust-1-bookworm AS chef
WORKDIR /app

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
# Build dependencies - this is the caching Docker layer!
RUN cargo chef cook --release --recipe-path recipe.json
# Build application
COPY . .
RUN cargo build --release --bin git-ents-server

# We do not need the Rust toolchain to run the binary!
FROM debian:bookworm-slim AS runtime
WORKDIR /app
# openssh-client provides the `ssh-keygen -Y verify` the pre-receive hook runs.
RUN apt-get update \
    && apt-get install -y --no-install-recommends git ca-certificates openssh-client \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/git-ents-server /usr/local/bin
COPY hooks /app/hooks
RUN chmod +x /app/hooks/pre-receive
ENV GIT_ENTS_HOOKS_DIR=/app/hooks
ENTRYPOINT ["/usr/local/bin/git-ents-server"]
