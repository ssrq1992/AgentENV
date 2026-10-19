#!/usr/bin/env bash
set -euo pipefail
[[ $(uname -s) == Linux ]] || { echo 'NOT RUN: Linux required'; exit 77; }
for cmd in go cargo docker kubectl curl jq openssl mkfs.ext4 blkid; do command -v "$cmd" >/dev/null || { echo "NOT RUN: missing $cmd"; exit 77; }; done
[[ -c /dev/kvm && -c /dev/ublk-control ]] || { echo 'NOT RUN: /dev/kvm and /dev/ublk-control required'; exit 77; }
go version
cargo --version
uname -r
kubectl version -o json
printf '%s\n' 'Preflight passed; this is not a VM test result.'
