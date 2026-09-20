#!/usr/bin/env bash
set -euo pipefail
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
MANIFEST_PATH="$REPO_ROOT/tools/zeroclaw-workspace/Cargo.toml"
cargo fmt --manifest-path "$MANIFEST_PATH" --all -- --check
cargo clippy --locked --manifest-path "$MANIFEST_PATH" --all-targets --all-features -- -D warnings
cargo test --locked --manifest-path "$MANIFEST_PATH"
