# syntax=docker.io/docker/dockerfile:1.7-labs

FROM lukemathwalker/cargo-chef:latest-rust-1 AS chef
WORKDIR /app
LABEL org.opencontainers.image.source=https://github.com/hl-archive-node/nanoreth
LABEL org.opencontainers.image.licenses="MIT OR Apache-2.0"

# Install system dependencies.
#   libclang-dev, pkg-config : bindgen / native crates
#   m4                       : gmp-mpfr-sys configure, required by the `gmp` feature
RUN apt-get update && apt-get -y upgrade && \
    apt-get install -y libclang-dev pkg-config m4 && \
    rm -rf /var/lib/apt/lists/*

# revmc's JIT is not a default feature: it needs LLVM matching inkwell's pinned major version
# (22.1), which is newer than the base image's distribution carries. To build an image with it,
# supply a base image that has llvm-22-dev and pass --build-arg ENABLE_JIT=true.
ARG ENABLE_JIT=false
ENV ENABLE_JIT=$ENABLE_JIT
ENV LLVM_SYS_221_PREFIX=/usr/lib/llvm-22

# Builds a cargo-chef plan
FROM chef AS planner
COPY --exclude=dist . /app/nanoreth
WORKDIR /app/nanoreth
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /app/nanoreth/recipe.json /app/nanoreth/recipe.json

# Build profile, release by default
ARG BUILD_PROFILE=release
ENV BUILD_PROFILE=$BUILD_PROFILE

# Extra Cargo flags
ARG RUSTFLAGS=""
ENV RUSTFLAGS="$RUSTFLAGS"

# Extra Cargo features
ARG FEATURES=""
ENV FEATURES=$FEATURES

# Builds dependencies
WORKDIR /app/nanoreth
# `target` and the cargo download caches are mounted rather than left in the layer. Without this,
# each RUN commits its whole filesystem delta: cooking reth's dependency tree wrote a single 74GB
# layer, because `lto = "thin"` makes every rlib carry LLVM bitcode alongside its object code.
# The mounts keep that data reusable across builds without it entering the image at all.
#
# cargo-chef cooks a skeleton before the real sources (and `.git`) are copied in, so vergen has
# no repository to read. VERGEN_IDEMPOTENT makes it emit placeholders for this layer only; the
# application build below runs without it and records the real commit.
RUN --mount=type=cache,id=nanoreth-target,target=/app/nanoreth/target,sharing=locked \
    --mount=type=cache,id=cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=cargo-git,target=/usr/local/cargo/git,sharing=locked \
    if [ "$ENABLE_JIT" = "true" ]; then JIT="--features jit"; else JIT=""; fi; \
    VERGEN_IDEMPOTENT=1 cargo chef cook --profile $BUILD_PROFILE $JIT --features "$FEATURES" --recipe-path recipe.json

# Build application.
COPY --exclude=dist . /app/nanoreth
# The checkout is owned by the host user, so libgit2 refuses to read it as root and vergen would
# fall back to a placeholder commit. Marking it safe keeps the real SHA in the version string.
RUN git config --global --add safe.directory /app/nanoreth
WORKDIR /app/nanoreth
# Same mounts, same ids, so this reuses what `chef cook` warmed above.
#
# The `cp` has to run here rather than in its own RUN: a cache mount is not part of the layer, so
# `target` no longer exists once this step ends. Copying to /app/reth-hl -- outside the mount --
# is what makes the binary visible to the runtime stage below. (ARG is still not resolved in COPY,
# which is why the fixed path is needed at all.)
RUN --mount=type=cache,id=nanoreth-target,target=/app/nanoreth/target,sharing=locked \
    --mount=type=cache,id=cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=cargo-git,target=/usr/local/cargo/git,sharing=locked \
    if [ "$ENABLE_JIT" = "true" ]; then JIT="--features jit"; else JIT=""; fi; \
    cargo build --profile $BUILD_PROFILE $JIT --features "$FEATURES" --locked --bin reth-hl && \
    cp /app/nanoreth/target/$BUILD_PROFILE/reth-hl /app/reth-hl

# Use Ubuntu as the release image
FROM ubuntu AS runtime
WORKDIR /app

# Install root certificates for aws sdk to work
RUN apt-get update && apt-get install -y ca-certificates && update-ca-certificates

# Copy reth over from the build stage
COPY --from=builder /app/reth-hl /usr/local/bin

# Copy licenses
COPY LICENSE-* ./

EXPOSE 9001 8545 8546
ENTRYPOINT ["/usr/local/bin/reth-hl"]