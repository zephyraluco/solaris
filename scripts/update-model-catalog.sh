#!/usr/bin/env bash
# Uses the same adapters as runtime refresh; requires curl and the workspace Rust toolchain.
set -euo pipefail
cd "$(dirname "$0")/.."
work=$(mktemp -d)
trap 'rm -rf -- "$work"' EXIT
curl --fail --location --silent --show-error --max-time 60 \
  https://models.dev/experimental/v2.0/api.json -o "$work/models-dev.json"
curl --fail --location --silent --show-error --max-time 60 \
  https://openrouter.ai/api/v1/models -o "$work/openrouter.json"
cargo run --locked -p solaris-provider --example generate_catalog -- \
  "$work/models-dev.json" "$work/openrouter.json" \
  crates/solaris-provider/src/catalog/snapshot.json
