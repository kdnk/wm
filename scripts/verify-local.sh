#!/usr/bin/env bash
# Modified by kdnk for wm on 2026-11-17. Original attribution is preserved in LICENSE.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd -P)
cd "$root"

if (( $# != 0 )); then
    echo "usage: scripts/verify-local.sh" >&2
    exit 2
fi

run_cargo() {
    local executable
    executable=$(rustup which --toolchain "$1" cargo)
    shift
    PATH="$(dirname "$executable"):$PATH" "$executable" "$@"
}

run_cargo 1.95.0 check --locked --workspace --all-targets
run_cargo nightly fmt --all --check
run_cargo stable check --locked --workspace
python3 tests/rift-settings-focus-verifier-test.py
python3 tests/rift-multi-resize-verifier-test.py
python3 tests/motion-content-verifier-test.py
python3 scripts/check-fork-notices.py
python3 tests/release-signing-setup-test.py
python3 tests/release-workflow-test.py
python3 tests/install-release-test.py
run_cargo stable test --locked --manifest-path cgs/Cargo.toml --lib exact_capture -- --test-threads=1
run_cargo stable test --locked --workspace --exclude cgs -- --test-threads=1
