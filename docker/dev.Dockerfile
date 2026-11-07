FROM rust:1.96.0-bookworm
RUN rustup component add rustfmt clippy \
 && cargo install sqlx-cli --version 0.9.0 --no-default-features --features rustls,postgres --locked \
 && cargo install cargo-deny --locked

# Node and pnpm for the embedded web UI (Task 14 onwards): pnpm install, vite build,
# openapi-typescript, and the bundle/license budget scripts all run in here, because the
# Windows host cannot be part of the build.
# The tarball is checked against nodejs.org's own SHASUMS256.txt, fetched over HTTPS. That
# detects a corrupted or truncated download, not a compromised nodejs.org; verifying the
# release signature would need the Node release GPG keys pinned in this image too.
ARG NODE_VERSION=22.11.0
ARG PNPM_VERSION=9.15.0
RUN set -eux; \
    arch="$(dpkg --print-architecture)"; \
    case "$arch" in \
      amd64) nodearch=x64 ;; \
      arm64) nodearch=arm64 ;; \
      *) echo "unsupported architecture: $arch" >&2; exit 1 ;; \
    esac; \
    cd /tmp; \
    curl -fsSLO "https://nodejs.org/dist/v${NODE_VERSION}/node-v${NODE_VERSION}-linux-${nodearch}.tar.xz"; \
    curl -fsSLO "https://nodejs.org/dist/v${NODE_VERSION}/SHASUMS256.txt"; \
    grep " node-v${NODE_VERSION}-linux-${nodearch}.tar.xz\$" SHASUMS256.txt | sha256sum -c -; \
    tar -xJf "node-v${NODE_VERSION}-linux-${nodearch}.tar.xz" -C /usr/local --strip-components=1 --no-same-owner; \
    rm -f "node-v${NODE_VERSION}-linux-${nodearch}.tar.xz" SHASUMS256.txt; \
    corepack enable; \
    corepack prepare "pnpm@${PNPM_VERSION}" --activate; \
    node --version; \
    pnpm --version

# Playwright for the Task 15 end-to-end suite. Both halves must be baked into the image: the
# dev container runs with `--rm`, so a browser or apt package installed during a run is gone
# the moment it exits, and `playwright install` would re-download on every single test run.
# PLAYWRIGHT_BROWSERS_PATH puts the browser at a fixed path instead of a per-user cache.
# Keep PLAYWRIGHT_VERSION and web/package.json's @playwright/test pin in lockstep: Playwright
# refuses to drive a browser build it did not ship with.
ENV PLAYWRIGHT_BROWSERS_PATH=/ms-playwright
ARG PLAYWRIGHT_VERSION=1.65.0
RUN npx --yes "playwright@${PLAYWRIGHT_VERSION}" install --with-deps chromium  && rm -rf /root/.npm /var/lib/apt/lists/*

WORKDIR /work
