#!/usr/bin/env bash
# Generate Python API models from openapi.json into the converse package.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
export PATH="${HOME}/.local/bin:${PATH}"
uv run --project services/converse datamodel-codegen \
  --input openapi.json \
  --input-file-type openapi \
  --disable-timestamp \
  --output services/converse/src/converse/api_models.py
echo "wrote services/converse/src/converse/api_models.py"
