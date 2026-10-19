#!/usr/bin/env bash
set -euo pipefail
ssf=$(cd "$(dirname "$0")/../../.." && pwd)
cd "$ssf"
[[ $(uname -s) == Linux ]] || { echo 'NOT RUN: Linux required'; exit 77; }
mkdir -p adapter/agentenvadapter/validation/linux
name="ssf-adapter-pg-$$"
docker run -d --name "$name" -e POSTGRES_PASSWORD=ssf-test-only -e POSTGRES_DB=ssf -p 127.0.0.1::5432 postgres:17.4 >/dev/null
trap 'docker rm -f "$name" >/dev/null' EXIT
for i in $(seq 1 60); do docker exec "$name" pg_isready -U postgres >/dev/null 2>&1 && break; sleep 1; done
port=$(docker port "$name" 5432/tcp | awk -F: '{print $NF}')
export SSF_TEST_DATABASE_URL="postgres://postgres:ssf-test-only@127.0.0.1:$port/ssf?sslmode=disable"
export SSF_ADAPTER_TEST_DATABASE_URL="$SSF_TEST_DATABASE_URL"
go test -race -count=1 -json ./adapter/agentenvadapter/... ./ledger ./consume > adapter/agentenvadapter/validation/linux/postgres.jsonl
docker build -f testing/Dockerfile -t ssf-test:0.1.0 .
docker run --rm --privileged --entrypoint bash -v "$PWD/adapter/agentenvadapter/validation/linux:/src/test-results" ssf-test:0.1.0 scripts/linux-mount-tests.sh
# The AgentENV image build uses the patched source and the original Cargo.lock.
bash adapter/agentenvadapter/scripts/build-agentenv.sh
printf '%s\n' 'PostgreSQL and Linux mount checks completed; VM E2E is a separate vm-e2e.py run against the deployed stack.'
