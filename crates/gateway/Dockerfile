# Global build args
ARG CARGO_BUILD_FLAGS=""
ARG GATEWAY_FEATURES=""
ARG GATEWAY_NO_DEFAULT_FEATURES="0"
ARG PROFILE="dev"

# ========== builder ==========

FROM lukemathwalker/cargo-chef:latest-rust-1.93-trixie AS chef

WORKDIR /src

FROM chef AS planner
WORKDIR /app/crates
COPY crates/ .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
ARG CARGO_BUILD_FLAGS
ARG GATEWAY_FEATURES
ARG GATEWAY_NO_DEFAULT_FEATURES
ARG PROFILE
WORKDIR /tensorzero
ENV CARGO_INCREMENTAL=0
ENV SKIP_TSC_VALIDATION=1
ARG CACHEBUST=1
RUN apt-get update && apt-get install -y clang libc++-dev && rm -rf /var/lib/apt/lists/*
COPY --from=planner /app/crates/recipe.json /tensorzero/crates/recipe.json
COPY crates/rust-toolchain.toml /tensorzero/crates/rust-toolchain.toml
WORKDIR /tensorzero/crates
RUN if [ "${GATEWAY_NO_DEFAULT_FEATURES}" = "1" ]; then \
        FEATURE_FLAGS="--no-default-features"; \
    else \
        FEATURE_FLAGS=""; \
    fi; \
    if [ -n "${GATEWAY_FEATURES}" ]; then \
        FEATURE_FLAGS="${FEATURE_FLAGS} --features ${GATEWAY_FEATURES}"; \
    fi; \
    cargo chef cook --profile ${PROFILE} -p gateway ${FEATURE_FLAGS} ${CARGO_BUILD_FLAGS}
COPY crates/ /tensorzero/crates/
RUN if [ "${GATEWAY_NO_DEFAULT_FEATURES}" = "1" ]; then \
        FEATURE_FLAGS="--no-default-features"; \
    else \
        FEATURE_FLAGS=""; \
    fi; \
    if [ -n "${GATEWAY_FEATURES}" ]; then \
        FEATURE_FLAGS="${FEATURE_FLAGS} --features ${GATEWAY_FEATURES}"; \
    fi; \
    cargo build --profile ${PROFILE} -p gateway ${FEATURE_FLAGS} ${CARGO_BUILD_FLAGS}
RUN if [ "${PROFILE}" = "dev" ]; then cp -r /tensorzero/crates/target/debug /release; else cp -r /tensorzero/crates/target/${PROFILE} /release; fi

# ========== base ==========

FROM debian:trixie-slim AS base

RUN apt-get update && apt-get install -y ca-certificates openssl wget curl && rm -rf /var/lib/apt/lists/*

# ========== gateway ==========

FROM base AS gateway

RUN useradd -m -s /bin/bash gateway

USER gateway

COPY --from=builder /release/gateway /usr/local/bin/gateway

WORKDIR /app

EXPOSE 3000

ENTRYPOINT ["gateway"]
