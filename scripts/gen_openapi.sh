#!/usr/bin/env bash
# Offline OpenAPI export: sorted keys so CI git-diff is deterministic.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
cargo run -q -p adapters --example gen_openapi | jq -S . > openapi.json
echo "wrote openapi.json"
