#
# Targets:  server (the engine + HTTP API)   tui (the terminal client)   mcpc (the MCP sign-in broker: mcpc, as its own user)
#
# Build cache mounts keep incremental image builds fast.

FROM docker.arvancloud.ir/rust:1-bookworm AS build
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
FROM docker.arvancloud.ir/debian:bookworm-slim AS runtime-base

RUN sed -i 's|http://deb.debian.org/debian|https://mirrors.tuna.tsinghua.edu.cn/debian|g' /etc/apt/sources.list.d/debian.sources \
    && sed -i 's|http://deb.debian.org/debian-security|https://mirrors.tuna.tsinghua.edu.cn/debian-security|g' /etc/apt/sources.list.d/debian.sources

# Temporarily use HTTP to bootstrap trusted certificates
RUN sed -i 's|https://|http://|g' /etc/apt/sources.list.d/debian.sources \
    && apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && update-ca-certificates \
    && sed -i 's|http://|https://|g' /etc/apt/sources.list.d/debian.sources \
    && apt-get update

RUN apt-get update \
 && apt-get install -y --no-install-recommends tini procps findutils diffutils grep coreutils \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --uid 10001 --create-home --shell /usr/sbin/nologin dfae \
 && mkdir -p /workspace && chown dfae:dfae /workspace

# --- engine ---------------------------------------------------------------------
# The agent's shell runs inside this container (see config/policy.toml: shell.mode = "sandbox"), so it gets
# the tools a developer expects. DEV_TOOLS=0 builds the small image (the shell then has only coreutils).
FROM runtime-base AS server
ARG DEV_TOOLS=1
RUN if [ "$DEV_TOOLS" = "1" ]; then \
      apt-get update \
   && apt-get install -y --no-install-recommends bash git curl jq ripgrep patch make gcc g++ libc6-dev python3 python3-venv python3-pip \
        file less tree unzip zip tar gzip xz-utils sed gawk openssh-client \
   && rm -rf /var/lib/apt/lists/*; \
    fi
# Node.js: the newest long-term-support release from nodejs.org (Debian's package is Node 18, too old for current frameworks).
# NODE_VERSION=lts picks the newest LTS at build time; pin one with e.g. --build-arg NODE_VERSION=v24.22.3.
ARG NODE_VERSION=lts
COPY deploy/docker/install-node.sh /tmp/install-node.sh
RUN if [ "$DEV_TOOLS" = "1" ]; then /tmp/install-node.sh "$NODE_VERSION" /usr/local; fi; rm -f /tmp/install-node.sh
# The validator's browser: agent-browser (headless Chrome for Testing driven through its accessibility tree) uses the pages the agent
# builds like a user would. Chrome goes to /opt/agent-browser so the unprivileged user can run it; UI_TOOLS=0 leaves it out (~450 MB).
ARG UI_TOOLS=1
ARG AGENT_BROWSER_VERSION=0.45.0
RUN if [ "$DEV_TOOLS" = "1" ] && [ "$UI_TOOLS" = "1" ]; then \
      npm install -g "agent-browser@${AGENT_BROWSER_VERSION}" --no-audit --no-fund \
   && HOME=/opt/agent-browser agent-browser install --with-deps \
   && chrome="$(find /opt/agent-browser -type f -name chrome -perm -u+x | head -1)" \
   && test -n "$chrome" && ln -s "$chrome" /usr/local/bin/dfae-chrome \
   && chmod -R a+rX /opt/agent-browser \
   && rm -rf /var/lib/apt/lists/* /root/.npm; \
    fi
ENV AGENT_BROWSER_EXECUTABLE_PATH=/usr/local/bin/dfae-chrome
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

# --- MCP sign-in broker -------------------------------------------------------------
# mcpc (https://github.com/apify/mcpc) authenticates and holds the engine's remote MCP sessions. It runs here, not in the engine's
# container: without a keyring it keeps sign-ins in files, and the agent's commands run as the engine's user there. The engine
# reaches it with DFAE_MCPC_BROKER_TOKEN; each session's proxy wants a bearer token only the engine holds.
FROM runtime-base AS mcpc
ARG NODE_VERSION=lts
# the mcpc release the engine is tested with (dfae_mcp::mcpc::MCPC_VERSION)
ARG MCPC_VERSION=0.8.0
COPY deploy/docker/install-node.sh /tmp/install-node.sh
RUN apt-get update \
 && apt-get install -y --no-install-recommends curl jq xz-utils \
 && /tmp/install-node.sh "$NODE_VERSION" /usr/local \
 && npm install -g "@apify/mcpc@${MCPC_VERSION}" --no-audit --no-fund \
 && apt-get purge -y curl jq xz-utils && apt-get autoremove -y \
 && rm -rf /var/lib/apt/lists/* /root/.npm /tmp/install-node.sh \
 && useradd --uid 10002 --create-home --shell /usr/sbin/nologin mcpc \
 && mkdir -p /var/lib/mcpc && chown mcpc:mcpc /var/lib/mcpc && chmod 0700 /var/lib/mcpc
COPY --from=build /out/dfae-server /usr/local/bin/dfae-server
ENV DFAE_MCPC_HOME=/var/lib/mcpc \
    DFAE_MCPC_BROKER_BIND=0.0.0.0:8091 \
    DFAE_MCPC_RELAY_BIND=0.0.0.0:8092 \
    DFAE_BIND=127.0.0.1:8091
USER mcpc
EXPOSE 8091 8092
HEALTHCHECK --interval=5s --timeout=3s --start-period=10s --retries=12 CMD ["dfae-server", "healthcheck"]
ENTRYPOINT ["/usr/bin/tini", "--", "dfae-server", "mcpc-broker"]

# --- terminal client -------------------------------------------------------------
FROM runtime-base AS tui
COPY --from=build /out/dfae /usr/local/bin/dfae
ENV TERM=xterm-256color \
    DFAE_URL=http://dfae:8080
USER dfae
ENTRYPOINT ["dfae"]
