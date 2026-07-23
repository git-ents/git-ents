# syntax=docker/dockerfile:1
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
# musl-tools: the static link target below. git + zig: `libghostty-vt-sys`
# (behind acdc-converters-html's `terminal` feature) clones ghostty at a
# pinned commit and builds it with zig from its build script — the same
# toolchain CI installs via mlugg/setup-zig, pinned to the same 0.15.2.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        musl-tools git ca-certificates curl xz-utils \
    && rm -rf /var/lib/apt/lists/*
ARG ZIG_VERSION=0.15.2
RUN curl -fsSL "https://ziglang.org/download/${ZIG_VERSION}/zig-$(uname -m)-linux-${ZIG_VERSION}.tar.xz" \
    | tar -xJ -C /usr/local \
    && ln -s "/usr/local/zig-$(uname -m)-linux-${ZIG_VERSION}/zig" /usr/local/bin/zig
RUN rustup target add x86_64-unknown-linux-musl
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates crates
# Cache mounts, not layer caching: the downloaded-crate registry and
# cargo's own incremental `target/` survive across separate `flyctl
# deploy` runs from this machine (Depot's builder persists cache-mount
# contents independently of the image layers, which invalidate on every
# source change). Without this, a one-line edit anywhere under `crates/`
# would recompile the entire dependency graph from scratch every deploy.
# `sharing=locked` on `target/`: cargo already serializes writes to it
# with its own lock file, but a locked mount avoids relying on that
# alone if two builds ever did overlap on the same cache.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/src/target,sharing=locked \
    cargo build --release --locked --target x86_64-unknown-linux-musl -p git-ents \
    && cp target/x86_64-unknown-linux-musl/release/git-ents /tmp/git-ents

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
COPY --from=builder /tmp/git-ents /usr/local/bin/git-ents
RUN chmod +x /usr/local/bin/git-ents
COPY docker/nginx.conf /etc/git-ents/nginx.conf
COPY docker/entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod +x /usr/local/bin/entrypoint.sh
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
