# syntax=docker/dockerfile:1
#
# Full kite-pdf render service: both backends (Chromium HTML→PDF + Typst).
# Build from the REPO ROOT so the workspace manifests and crates are in context:
#
#   docker build -f crates/pdf/Dockerfile -t kite-pdf .
#   docker run -p 8091:8091 kite-pdf
#
# For a browser-free image less than half the size, use crates/pdf/Dockerfile.lite.

# ---- build stage: compile the `kite-pdf` binary (default = chromium + typst) --
FROM rust:1-bookworm AS build
WORKDIR /src

COPY Cargo.toml Cargo.lock ./
COPY crates ./crates

RUN cargo build --release -p kite-pdf && \
    strip target/release/kite-pdf || true

# ---- runtime stage: slim Debian + Chromium ----------------------------------
FROM debian:bookworm-slim AS runtime

# Chromium (for the HTML backend) + CA certs + fonts so rendered pages/PDFs are
# not blank. The Typst backend bundles its own fonts into the binary.
RUN apt-get update && \
    apt-get install -y --no-install-recommends \
        chromium \
        ca-certificates \
        fonts-liberation && \
    rm -rf /var/lib/apt/lists/*

# Binds all interfaces so the container is reachable. Because that's network-
# exposed, the server FAILS TO START without auth: run with
# `-e KITE_PDF_AUTH_TOKEN=<token>` (recommended), or `-e KITE_PDF_INSECURE=1` to
# override on a trusted network. The render endpoint can fetch on your behalf, so
# keep it behind auth + network policy (see KITE_PDF_ALLOW_PRIVATE_IPS).
ENV BROWSER_EXECUTABLE=/usr/bin/chromium \
    BROWSER_NO_SANDBOX=1 \
    KITE_HEADLESS=1 \
    KITE_PDF_BIND=0.0.0.0:8091 \
    KITE_CACHE_DIR=/home/kite/.cache/kitewright

RUN useradd --create-home --uid 10001 kite
COPY --from=build /src/target/release/kite-pdf /usr/local/bin/kite-pdf

USER kite
WORKDIR /home/kite
EXPOSE 8091

# No args → serves the HTTP render service on :8091.
ENTRYPOINT ["kite-pdf"]
