# The single-node hosted root (`roots.single-node-hosted`,
# docs/development-plan.adoc phase 6): the `git-ents` binary itself, wired
# so stock git's own smart-HTTP transport (`git http-backend`, via
# nginx+fcgiwrap) invokes its `pre-receive`/`post-receive` hooks. No
# Postgres/Tigris/gix-receive here — that is `git-ents-server`, phase 8.
#
# The binary builds *in* this image, from the same source tree the rest
# of the deploy comes from — never a separately cross-compiled artifact
# copied in by hand. That used to be `docker/bin/git-ents`, a musl binary
# built on the host and materialized here before `docker build`; the
# whole point of that indirection was to skip a slow in-container Rust
# build, but it silently deployed stale code whenever someone forgot the
# manual rebuild step, exactly the failure mode a deploy pipeline exists
# to prevent.
FROM rust:1-slim-bookworm AS builder
RUN apt-get update \
    && apt-get install -y --no-install-recommends musl-tools \
    && rm -rf /var/lib/apt/lists/*
RUN rustup target add x86_64-unknown-linux-musl
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates crates
RUN cargo build --release --locked --target x86_64-unknown-linux-musl -p git-ents

FROM debian:bookworm-slim AS runtime
WORKDIR /app
# git: the bare repo + git-http-backend CGI itself.
# nginx+fcgiwrap+spawn-fcgi: the smart-HTTP transport (Phase 0's bootstrap,
# still the transport Phase 6 rides per docs/development-plan.adoc).
# curl: installs the sprite CLI the post-receive hook shells out to.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        git ca-certificates curl nginx fcgiwrap spawn-fcgi \
    && rm -rf /var/lib/apt/lists/*
# The sprite CLI runs post-receive's checks in a Sprite; it reads
# SPRITES_TOKEN from the env. The installer drops the binary in
# $HOME/.local/bin and never touches PATH, so point it at /usr/local/bin
# (already on PATH) where the hosted root can spawn it.
RUN curl -fsSL https://sprites.dev/install.sh \
    | env SPRITE_INSTALL_PREFERRED_DIRS=/usr/local/bin \
          SPRITE_INSTALL_DEFAULT_BIN_DIR=/usr/local/bin bash
COPY --from=builder /src/target/x86_64-unknown-linux-musl/release/git-ents /usr/local/bin/git-ents
RUN chmod +x /usr/local/bin/git-ents
COPY docker/nginx.conf /etc/git-ents/nginx.conf
COPY docker/entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod +x /usr/local/bin/entrypoint.sh
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
