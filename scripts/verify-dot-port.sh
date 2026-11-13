#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd -P)
cd "$root"

if (( $# > 1 )); then
    echo "usage: scripts/verify-dot-port.sh [CONFIG.toml]" >&2
    exit 2
fi

python3 tests/rift-settings-focus-verifier-test.py
python3 tests/rift-multi-resize-verifier-test.py
cargo test --locked --manifest-path cgs/Cargo.toml --lib exact_capture -- --test-threads=1
cargo test --locked --lib -- --test-threads=1
cargo run --locked --example config-check -- "${1:-rift.default.toml}"
cargo run --locked --example menu-bar-window-list-check
cargo build --locked --bin rift --bin rift-cli
RIFT_SERVICE_TEST_BINARY="$(cargo metadata --locked --no-deps --format-version 1 | python3 -c 'import json, sys; print(json.load(sys.stdin)["target_directory"] + "/debug/rift")')" \
    bash tests/rift-patched-service-test.sh
