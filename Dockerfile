# Copyright 2026 Manu P Anand
# Licensed under the Apache License, Version 2.0
# See LICENSE for details.

# Targets:
#   runtime (default)  slim image running `fornax serve` as an unprivileged user
#   test               runs the full workspace test suite (needs git, ssh, openssl)

ARG NODE_IMAGE=node:24-bookworm-slim
ARG RUST_IMAGE=rust:1-bookworm
ARG RUNTIME_IMAGE=debian:bookworm-slim

# ---- web UI (Svelte) -----------------------------------------------------------------
FROM ${NODE_IMAGE} AS web
WORKDIR /web
COPY web/package.json web/package-lock.json ./
RUN npm ci --no-audit --no-fund
COPY web/ ./
RUN npm run check && npm run build

# ---- build ---------------------------------------------------------------------------
FROM ${RUST_IMAGE} AS build
WORKDIR /src
COPY . .
COPY --from=web /web/dist web/dist
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p fornax-server \
 && install -m 0755 target/release/fornax /usr/local/bin/fornax

# ---- test ----------------------------------------------------------------------------
FROM ${RUST_IMAGE} AS test
RUN apt-get update \
 && apt-get install -y --no-install-recommends openssh-client \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
COPY --from=web /web/dist web/dist
CMD ["cargo", "test", "--workspace", "--locked"]

# ---- runtime -------------------------------------------------------------------------
FROM ${RUNTIME_IMAGE} AS runtime
RUN apt-get update \
 && apt-get install -y --no-install-recommends git ca-certificates curl \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 --user-group --home-dir /data --shell /usr/sbin/nologin fornax \
 && mkdir -p /data \
 && chown fornax:fornax /data
COPY --from=build /usr/local/bin/fornax /usr/local/bin/fornax
USER fornax
ENV FORNAX_DATA_DIR=/data \
    FORNAX_LISTEN=0.0.0.0:3000 \
    FORNAX_SSH_LISTEN=0.0.0.0:2222 \
    RUST_LOG=info
VOLUME ["/data"]
EXPOSE 3000 2222
HEALTHCHECK --interval=10s --timeout=3s --start-period=5s --retries=3 \
  CMD curl -fsS http://127.0.0.1:3000/api/v1/health || exit 1
ENTRYPOINT ["fornax"]
CMD ["serve"]
