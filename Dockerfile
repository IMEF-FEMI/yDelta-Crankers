# ydelta-crankers — production image
#
# Multi-stage so the runtime image doesn't ship rustc + ~6GB of cargo cache.
# Build context: the crankers/ directory itself. Cargo fetches the
# `ydelta` + `hypertree` deps from github.com/IMEF-FEMI/yDelta at the
# revision pinned in Cargo.toml, so the build doesn't need any sibling
# directories.
#
# From crankers/ build with:
#   docker build -t ydelta-crankers .

# ─── Builder ─────────────────────────────────────────────────────────
FROM rust:1.90-slim AS builder
WORKDIR /build

# System deps. pkg-config + libssl-dev for transitive crates; git so
# cargo can fetch the ydelta git dep; ca-certificates for HTTPS.
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config libssl-dev ca-certificates git \
    && rm -rf /var/lib/apt/lists/*

COPY . /build/
RUN cargo build --release --bin ydelta-crankers

# ─── Runtime ─────────────────────────────────────────────────────────
FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Non-root user. Keypair files mounted at /secrets/ should be owned by
# this UID (chown 10001 /secrets in the deploy step).
RUN useradd -u 10001 -r -s /sbin/nologin cranker
USER cranker
WORKDIR /home/cranker

COPY --from=builder /build/target/release/ydelta-crankers /usr/local/bin/ydelta-crankers

# Prometheus exporter port. Override via METRICS_BIND if the platform
# (e.g. Railway) injects a different PORT.
EXPOSE 9091

# Railway / k8s deliver SIGTERM; the binary's tokio signal handler
# already handles graceful shutdown.
ENTRYPOINT ["/usr/local/bin/ydelta-crankers"]
