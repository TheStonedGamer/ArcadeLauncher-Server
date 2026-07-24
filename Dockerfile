# Multi-stage build for the ArcadeLauncher server.
#
# The binary is fully env-configurable (Config::from_env in src/models.rs), so
# the container needs no CLI flags — every setting comes from the pod's env.
# All TLS is rustls (no OpenSSL), so the runtime image only needs CA certs.
# Build on the workstation Docker Desktop and push brianthemint/arcadelauncher-server.

# --- builder -----------------------------------------------------------------
FROM rust:1-bookworm AS builder
# cmake + build-essential: the rustls/aws-lc-sys crypto backend builds native
# code; pkg-config for crate build scripts. ca-certs for cargo fetch over TLS.
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential pkg-config cmake ca-certificates \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /build

# Single clean build. (No stub-cache split: the crate is one binary that pulls in
# its modules via include!, which the stub trick mis-caches.) A BuildKit cache
# mount keeps the cargo registry + target dir warm across rebuilds instead.
COPY Cargo.toml Cargo.lock VERSION ./
COPY src ./src
# src/auth.rs does include_str!("../VERSION"), so VERSION must sit beside src/.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    cargo build --release --locked \
    && cp target/release/arcadelauncher-server /usr/local/bin/arcadelauncher-server

# --- runtime -----------------------------------------------------------------
FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --home /opt/arcade arcade
WORKDIR /opt/arcade
COPY --from=builder /usr/local/bin/arcadelauncher-server /usr/local/bin/arcadelauncher-server
USER arcade
# Public API 8721, admin 8722 (expose admin only via an internal Service).
EXPOSE 8721 8722
ENTRYPOINT ["/usr/local/bin/arcadelauncher-server"]
