# syntax=docker/dockerfile:1

# ── Stage 1: dependency cache using cargo-chef ──────────────────────────────
FROM rust:slim-bookworm AS chef
RUN cargo install cargo-chef --locked
WORKDIR /app

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# ── Stage 2: build ──────────────────────────────────────────────────────────
FROM chef AS builder
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

COPY --from=planner /app/recipe.json recipe.json
# Build dependencies only (cached layer)
RUN cargo chef cook --release --recipe-path recipe.json

COPY . .
RUN cargo build --release

# ── Stage 3: minimal runtime image ──────────────────────────────────────────
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/renovate-k8s-trigger /usr/local/bin/renovate-k8s-trigger

EXPOSE 8080
USER 65534
ENTRYPOINT ["renovate-k8s-trigger"]
