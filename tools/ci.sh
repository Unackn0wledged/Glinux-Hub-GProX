#!/usr/bin/env bash
# Local equivalent of .github/workflows/ci.yml. Usage: bash tools/ci.sh
set -euo pipefail
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
echo "ci: OK"
