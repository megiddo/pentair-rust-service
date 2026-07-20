# Canonical Linux toolchain for pentairservice (Debian bookworm, headless).
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates curl build-essential pkg-config \
    && rm -rf /var/lib/apt/lists/*

# Pin a stable Rust channel for reproducible CI/agent runs
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y \
    --default-toolchain stable --profile minimal \
    && . "$HOME/.cargo/env" \
    && rustup component add llvm-tools-preview \
    && cargo install cargo-llvm-cov

ENV PATH="/root/.cargo/bin:${PATH}"
WORKDIR /workspace
