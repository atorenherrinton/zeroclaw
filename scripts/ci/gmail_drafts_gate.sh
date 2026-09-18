#!/usr/bin/env bash

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
MANIFEST_PATH="$REPO_ROOT/tools/zeroclaw-gmail/Cargo.toml"

echo "==> Gmail drafts: checking formatting"
cargo fmt --manifest-path "$MANIFEST_PATH" --all -- --check

echo "==> Gmail drafts: running strict Clippy"
cargo clippy --locked --manifest-path "$MANIFEST_PATH" --all-targets --all-features -- -D warnings

echo "==> Gmail drafts: running locked tests"
cargo test --locked --manifest-path "$MANIFEST_PATH"
