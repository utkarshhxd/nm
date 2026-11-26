# Production image: one `nm` binary with the web UI compiled in, on a slim runtime.
#   docker build -f docker/Dockerfile -t nm .
# Build context is the repo root (see /.dockerignore).

# ---- 1. Web UI -> web/dist ------------------------------------------------------------------
FROM node:22.11.0-bookworm-slim AS web
ARG PNPM_VERSION=9.15.0
RUN corepack enable && corepack prepare "pnpm@${PNPM_VERSION}" --activate
WORKDIR /src/web
COPY web/package.json web/pnpm-lock.yaml ./
RUN pnpm install --frozen-lockfile
COPY web/ ./
RUN pnpm build

# ---- 2. Rust binary, UI embedded (rust-embed reads web/dist at compile time) ----------------
# Keep the tag in step with rust-toolchain.toml.
FROM rust:1.96.0-bookworm AS build
# The build VM has 8 GB: raise this only if it has room for more rustc processes.
ARG CARGO_BUILD_JOBS=2
ENV CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS} SQLX_OFFLINE=true
WORKDIR /src
COPY . .
COPY --from=web /src/web/dist web/dist
RUN cargo build --release --locked --bin nm

# ---- 3. Runtime -----------------------------------------------------------------------------
FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 --no-create-home --shell /usr/sbin/nologin nm
COPY --from=build /src/target/release/nm /usr/local/bin/nm
USER 10001:10001
EXPOSE 8080
ENTRYPOINT ["nm"]
# Same as running `nm` with no subcommand; compose and Kubernetes override it with `all` etc.
CMD ["serve"]
