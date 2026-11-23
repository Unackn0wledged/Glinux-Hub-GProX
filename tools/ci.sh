#!/usr/bin/env bash
# Local equivalent of .github/workflows/ci.yml. Usage: bash tools/ci.sh
#
# The golden and import tests compare against local reference data under re/
# (not distributed) and skip when it is absent, as on GitHub. With the data
# present, GPROX_REQUIRE_VECTORS=1 turns every skip into a failure, so a green
# run means every comparison ran (GPROX_RE_DIR points at another re/ folder).
set -euo pipefail
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
echo "ci: OK (GPROX_REQUIRE_VECTORS=${GPROX_REQUIRE_VECTORS:-0})"
