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

ARG HERDR_VERSION=latest
ARG CLAUDE_CODE_VERSION=latest
ARG CODEX_VERSION=latest
ARG PI_VERSION=latest

USER root

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
    crew-check
