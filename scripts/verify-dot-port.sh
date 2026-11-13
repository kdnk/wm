#!/usr/bin/env bash
# Modified by kdnk for wm on 2026-11-14. Original attribution is preserved in LICENSE.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd -P)
cd "$root"

if (( $# > 1 )); then
    echo "usage: scripts/verify-dot-port.sh [CONFIG.toml]" >&2
    exit 2
fi

python3 tests/rift-settings-focus-verifier-test.py
python3 tests/rift-multi-resize-verifier-test.py
python3 scripts/check-fork-notices.py
cargo test --locked --manifest-path cgs/Cargo.toml --lib exact_capture -- --test-threads=1
cargo test --locked --lib -- --test-threads=1
cargo test --locked --test wm_identity -- --test-threads=1
cargo run --locked --example config-check -- "${1:-wm.default.toml}"
cargo run --locked --example menu-bar-window-list-check
cargo build --locked --bin wm --bin wm-cli
binary_dir=$(cargo metadata --locked --no-deps --format-version 1 | python3 -c 'import json, sys; print(json.load(sys.stdin)["target_directory"] + "/debug")')
python3 tests/wm-cli-identity-test.py "$binary_dir"
WM_SERVICE_TEST_BINARY="$binary_dir/wm" \
    bash tests/rift-patched-service-test.sh
