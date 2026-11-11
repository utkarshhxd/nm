#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only
# Run a command in the Linux dev container: bash scripts/dev.sh cargo test
# MSYS_NO_PATHCONV stops Git Bash from mangling /work-style arguments.
cd "$(dirname "$0")/.." || exit 1
# Cap rustc's parallelism. The WSL2 VM this container runs in is capped at 4 GB by the host's
# ~/.wslconfig, and `cargo test` links 13 integration-test binaries at once. Unthrottled, that
# exhausts the VM and takes the whole Docker engine down with it — observed three times, each
# needing a Docker Desktop restart. This is the compile phase: `--test-threads` does not help,
# because the engine dies before a single test runs. Override with CARGO_BUILD_JOBS=N if the
# host's WSL memory limit is ever raised.
MSYS_NO_PATHCONV=1 exec docker compose -f docker-compose.dev.yml -p nm run --rm \
  -e CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-2}" dev "$@"
