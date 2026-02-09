#!/usr/bin/env bash
# Nextest setup script: start a single PostgreSQL container for all tests.
# Each test creates its own database within it via gator-test-utils.
#
# Uses a fixed container name. Each run cleans up the previous container
# (if any) before starting fresh. The container persists after this script
# exits and is cleaned up by the next run.
set -euo pipefail

CONTAINER_NAME="gator-nextest-pg"

# Clean up stale container from previous run.
docker rm -f "$CONTAINER_NAME" >/dev/null 2>&1 || true

docker run --rm -d \
    --name "$CONTAINER_NAME" \
    -e POSTGRES_USER=postgres \
    -e POSTGRES_PASSWORD=postgres \
    -e POSTGRES_HOST_AUTH_METHOD=trust \
    -p 5432 \
    postgres:18 \
    -c max_connections=500 \
    -c shared_buffers=256MB >/dev/null

# Wait for PostgreSQL to accept connections.
for _ in $(seq 1 30); do
    if docker exec "$CONTAINER_NAME" pg_isready -U postgres >/dev/null 2>&1; then
        break
    fi
    sleep 0.5
done

# Resolve the host-mapped port.
PG_PORT=$(docker port "$CONTAINER_NAME" 5432/tcp | head -1 | cut -d: -f2)

# Inject env var into every test process that depends on this script.
echo "GATOR_TEST_PG_URL=postgresql://postgres:postgres@127.0.0.1:${PG_PORT}" >> "$NEXTEST_ENV"
