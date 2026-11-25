# syntax=docker/dockerfile:1.7
#
# Targets:  server (the engine + HTTP API)   tui (the terminal client)
#
# Build cache mounts keep incremental image builds fast.

FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p dfae-server -p dfae-tui \
 && mkdir -p /out \
 && cp target/release/dfae-server target/release/dfae /out/

# --- shared runtime base ------------------------------------------------------
# coreutils/findutils/diffutils/grep/procps provide the allowlisted shell tools
# (ls, df, grep, find, ps, free, ...). There is deliberately no shell scripting
# toolchain, package manager access or compiler in the runtime image.
FROM debian:bookworm-slim AS runtime-base
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates tini procps findutils diffutils grep coreutils \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --uid 10001 --create-home --shell /usr/sbin/nologin dfae \
 && mkdir -p /workspace && chown dfae:dfae /workspace

# --- engine ---------------------------------------------------------------------
FROM runtime-base AS server
COPY --from=build /out/dfae-server /usr/local/bin/dfae-server
COPY config/policy.toml /etc/dfae/policy.toml
ENV DFAE_BIND=0.0.0.0:8080 \
    DFAE_WORKSPACE=/workspace \
    DFAE_POLICY=/etc/dfae/policy.toml
USER dfae
WORKDIR /workspace
EXPOSE 8080
HEALTHCHECK --interval=5s --timeout=3s --start-period=10s --retries=12 CMD ["dfae-server", "healthcheck"]
ENTRYPOINT ["/usr/bin/tini", "--", "dfae-server"]

# --- terminal client -------------------------------------------------------------
FROM runtime-base AS tui
COPY --from=build /out/dfae /usr/local/bin/dfae
ENV TERM=xterm-256color \
    DFAE_URL=http://dfae:8080
USER dfae
ENTRYPOINT ["dfae"]
