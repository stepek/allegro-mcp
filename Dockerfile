# syntax=docker/dockerfile:1

# ---- builder --------------------------------------------------------------
FROM rust:1.88-slim-bookworm AS builder
WORKDIR /app

# rustls-tls (reqwest) needs no OpenSSL; pkg-config/build-essential cover
# the rest of the crate graph's build-time needs.
RUN apt-get update \
    && apt-get install -y --no-install-recommends pkg-config \
    && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

# ---- runtime ----------------------------------------------------------------
FROM gcr.io/distroless/cc-debian12 AS runtime

# `dirs::config_dir()` (used for allegro-mcp.toml discovery and reserved
# for a future on-disk token cache) resolves via $HOME on Linux.
ENV HOME=/root
ENV PORT=8080

COPY --from=builder /app/target/release/allegro-mcp /usr/local/bin/allegro-mcp

EXPOSE 8080

# Distroless has no shell/curl — the binary probes itself (see
# `Commands::Healthcheck` in src/main.rs). start-period=30s (not 10s):
# startup does a real network round-trip (schema load + eager Allegro
# OAuth token fetch, see Phase 3.4) before the port is bound, and a cold
# TLS handshake against developer.allegro.pl / OAuth can comfortably
# exceed 10s on a loaded host or slow network — 10s risked flapping the
# container to "unhealthy" during a perfectly normal cold start.
HEALTHCHECK --interval=30s --timeout=3s --start-period=30s --retries=3 \
    CMD ["/usr/local/bin/allegro-mcp", "healthcheck"]

ENTRYPOINT ["/usr/local/bin/allegro-mcp"]
