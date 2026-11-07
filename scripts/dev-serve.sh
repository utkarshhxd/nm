#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only
# Like dev.sh but publishes port 8080 (compose options must precede the service name).
cd "$(dirname "$0")/.." || exit 1
MSYS_NO_PATHCONV=1 exec docker compose -f docker-compose.dev.yml -p nm run --rm --service-ports dev "$@"
