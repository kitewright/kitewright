# syntax=docker/dockerfile:1

# ---- build stage: compile the `kite` release binary --------------------------
FROM rust:1-bookworm AS build
WORKDIR /src

# Copy the workspace manifests first so a dependency layer can cache across
# source-only changes. (The napi Node addon is excluded from the workspace and
# from the build context via .dockerignore.)
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates

# Build only the server crate (produces the `kite` binary) in release mode.
RUN cargo build --release -p kitewright && \
    strip target/release/kite || true

# ---- runtime stage: slim Debian + Chromium ----------------------------------
FROM debian:bookworm-slim AS runtime

# Chromium + the shared libraries a headless browser needs, plus CA certs for
# HTTPS and a font package so rendered pages/PDFs are not blank.
RUN apt-get update && \
    apt-get install -y --no-install-recommends \
        chromium \
        ca-certificates \
        fonts-liberation && \
    rm -rf /var/lib/apt/lists/*

# Point the engine at the apt-installed Chromium and run it without the sandbox
# (required in most containers, where user namespaces are unavailable).
ENV BROWSER_EXECUTABLE=/usr/bin/chromium \
    BROWSER_NO_SANDBOX=1 \
    MCP_HTTP_BIND=0.0.0.0:8090 \
    KITE_CACHE_DIR=/home/kite/.cache/kitewright

# Non-root user (Chromium refuses to run as root without --no-sandbox anyway,
# and least-privilege is good practice for a networked service).
RUN useradd --create-home --uid 10001 kite
COPY --from=build /src/target/release/kite /usr/local/bin/kite

USER kite
WORKDIR /home/kite
EXPOSE 8090

# The engine installs its own SIGTERM handler and closes the browser child on
# `docker stop`, so no init shim is required. `kite` with no args serves MCP
# over Streamable HTTP on :8090; pass `--stdio` to override.
ENTRYPOINT ["kite"]
