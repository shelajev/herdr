# Herdr crew sandbox template.
#
# Bakes herdr + the three crew agent CLIs into a reusable sandbox image so
# `sbx create` with the herdr-crew kit is fast: the kit's install step only
# verifies the tools exist instead of downloading them. Build and load it
# with scripts/sbx/build-crew-template.sh (Docker Desktop required).
#
# Base per the kit spec requirements (non-root `agent` user at UID 1000 with
# passwordless sudo, /home/agent, proxy env preserved across sudo):
# https://docs.docker.com/ai/sandboxes/customize/kit-reference/#sandbox-block
FROM docker/sandbox-templates:shell-docker

# The inner Herdr is an ordinary upstream release and does not need the fork's
# task commands. Pinned so a template rebuild is reproducible rather than
# picking up whatever is newest; v0.9.3 is the verified upstream stable.
ARG HERDR_VERSION=0.9.3
ARG CLAUDE_CODE_VERSION=2.1.289
ARG CODEX_VERSION=0.153.4
ARG PI_VERSION=0.85.1

# Node.js runtime. The base image ships Node 20.19.4, below the engines floors
# of the pinned agent CLIs (checked 2026-10-04 from the installed packages):
#   @anthropic-ai/claude-code 2.1.289  engines.node >=22.0.0
#   @earendil-works/pi-coding-agent 0.85.1  engines.node >=22.19.0
#   @openai/codex 0.153.4  engines.node >=16
# so the official tarball is installed over it. crew-check enforces the same
# floors. The hashes below are the node-v22.22.1-linux-{x64,arm64}.tar.gz lines
# of https://nodejs.org/dist/v22.22.1/SHASUMS256.txt, fetched 2026-10-04 (the
# tarballs were also downloaded and hashed independently). When bumping
# NODE_VERSION, refresh both hashes from that file for the new version.
ARG NODE_VERSION=22.22.1
ARG NODE_SHA256_X64=07c8aafa60644fb81adefa1ee7da860eb1920851ffdc9a37020ab0be47fbc10e
ARG NODE_SHA256_ARM64=1d1690e9aba47e887a275abc6d8f7317e571a0700deaef493f768377e99155f5

USER root

# Pinned Node.js from nodejs.org, verified against the checksum above. The
# build fails (no fallback to the base image's Node) if nodejs.org is
# unreachable, the checksum differs, or the resulting node is not the pin.
RUN set -eux; \
    arch="$(uname -m)"; \
    case "$arch" in \
      x86_64) node_arch=x64; node_sha256="$NODE_SHA256_X64" ;; \
      aarch64 | arm64) node_arch=arm64; node_sha256="$NODE_SHA256_ARM64" ;; \
      *) echo "unsupported arch: $arch" >&2; exit 1 ;; \
    esac; \
    tarball="node-v${NODE_VERSION}-linux-${node_arch}.tar.gz"; \
    curl -fsSL "https://nodejs.org/dist/v${NODE_VERSION}/${tarball}" -o "/tmp/${tarball}"; \
    echo "${node_sha256}  /tmp/${tarball}" | sha256sum -c -; \
    tar -xzf "/tmp/${tarball}" -C /usr/local --strip-components=1 --no-same-owner; \
    rm -f "/tmp/${tarball}"; \
    test "$(/usr/local/bin/node --version)" = "v${NODE_VERSION}"; \
    test "$(node --version)" = "v${NODE_VERSION}"; \
    npm --version

# herdr release binary (linux x86_64/aarch64 assets).
RUN set -eux; \
    arch="$(uname -m)"; \
    case "$arch" in \
      x86_64) asset=herdr-linux-x86_64 ;; \
      aarch64 | arm64) asset=herdr-linux-aarch64 ;; \
      *) echo "unsupported arch: $arch" >&2; exit 1 ;; \
    esac; \
    if [ "$HERDR_VERSION" = "latest" ]; then \
      url="https://github.com/herdrdev/herdr/releases/latest/download/${asset}"; \
    else \
      url="https://github.com/herdrdev/herdr/releases/download/v${HERDR_VERSION}/${asset}"; \
    fi; \
    curl -fsSL "$url" -o /usr/local/bin/herdr; \
    chmod 0755 /usr/local/bin/herdr; \
    /usr/local/bin/herdr --version

# beans issue tracker (hmans/beans): lets the crew work off a beans backlog
# committed in the task workspace (.beans/).
ARG BEANS_VERSION=0.4.2
RUN set -eux; \
    arch="$(uname -m)"; \
    case "$arch" in \
      x86_64) beans_arch=x86_64 ;; \
      aarch64 | arm64) beans_arch=arm64 ;; \
      *) echo "unsupported arch: $arch" >&2; exit 1 ;; \
    esac; \
    curl -fsSL "https://github.com/hmans/beans/releases/download/v${BEANS_VERSION}/beans_Linux_${beans_arch}.tar.gz" \
      | tar -xz -C /usr/local/bin beans; \
    chmod 0755 /usr/local/bin/beans; \
    beans version

# Crew helper scripts. Baked into the image (not kit files) so they carry the
# executable bit and version together with the tools they manage.
COPY --chmod=0755 crew-entry crew-check /usr/local/bin/

# Agent CLIs install as the `agent` user into a user-writable npm prefix so
# their self-update paths keep working without root.
USER agent
ENV NPM_CONFIG_PREFIX=/home/agent/.local
ENV PATH=/home/agent/.local/bin:$PATH
# pi (pi.dev) is the Google-models crew member: provider google is its
# default, it reads GEMINI_API_KEY, and --model picks e.g. gemini-3.8-flash.
# --ignore-scripts follows pi's documented install (no lifecycle scripts).
RUN set -eux; \
    ver() { [ "$1" = "latest" ] && echo "" || echo "@$1"; }; \
    npm install -g \
      "@anthropic-ai/claude-code$(ver "$CLAUDE_CODE_VERSION")" \
      "@openai/codex$(ver "$CODEX_VERSION")"; \
    npm install -g --ignore-scripts \
      "@earendil-works/pi-coding-agent$(ver "$PI_VERSION")"; \
    test "$(node --version)" = "v${NODE_VERSION}"; \
    crew-check
