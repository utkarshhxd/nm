#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only
# Run a command in the Linux dev container: bash scripts/dev.sh cargo test
# MSYS_NO_PATHCONV stops Git Bash from mangling /work-style arguments.
cd "$(dirname "$0")/.." || exit 1
MSYS_NO_PATHCONV=1 exec docker compose -f docker-compose.dev.yml -p nm run --rm dev "$@"
