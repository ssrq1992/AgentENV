#!/usr/bin/env bash
set -euo pipefail
root=$(cd "$(dirname "$0")/../../../.." && pwd)
agent=${AGENTENV_DIR:-"$root/AgentENV"}
# Keep the project's Dockerfile intact, pin its Rust and cargo-chef build inputs.
file=$(mktemp "$agent/.adapter-Dockerfile.XXXXXX")
trap 'rm -f "$file"' EXIT
python3 - "$agent/deploy/docker/Dockerfile.agentenv" "$file" <<'PY'
import sys
s=open(sys.argv[1]).read().replace('FROM rust:1-bookworm AS chef','FROM rust:1.99.1-bookworm AS chef').replace('cargo install cargo-chef --locked','cargo install cargo-chef --version 0.1.71 --locked')
open(sys.argv[2],'w').write(s)
PY
docker build --build-arg AENV_VIRTUALIZATION_MODE=kvm -f "$file" -t agentenv-ssf:0.2.3 "$agent"
