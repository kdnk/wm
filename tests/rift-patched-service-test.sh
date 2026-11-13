#!/usr/bin/env bash
# Modified by kdnk for wm on 2026-11-14. Original attribution is preserved in LICENSE.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd -P)
source_binary="${WM_SERVICE_TEST_BINARY:-$root/target/debug/wm}"
if [[ ! -x "$source_binary" ]]; then
    echo "Set WM_SERVICE_TEST_BINARY to a built daemon, or run cargo build --bin wm." >&2
    exit 1
fi

scratch=$(mktemp -d)
scratch=$(cd "$scratch" && pwd -P)
trap 'rm -rf "$scratch"' EXIT
install_dir="$scratch/native & <cache> \"quoted\" &amp;"
fixture_home="$scratch/home"
fixture_user='fixture & <user> &amp;'
wrapper_dir="$scratch/tools & <utilities> &amp;/bin"
fixture_path="$wrapper_dir:$PATH"
mkdir -p "$install_dir" "$fixture_home" "$wrapper_dir"
cp "$source_binary" "$install_dir/wm"
cat > "$wrapper_dir/wm" <<'WRAPPER'
#!/bin/sh
exit 42
WRAPPER
chmod +x "$wrapper_dir/wm"
HOME="$fixture_home" USER="$fixture_user" PATH="$fixture_path" \
    "$install_dir/wm" service install

plist="$fixture_home/Library/LaunchAgents/git.kdnk.wm.plist"
/usr/bin/plutil -lint "$plist"
python3 - "$plist" "$install_dir" "$fixture_path" "$fixture_user" <<'PY'
import plistlib
import sys

with open(sys.argv[1], 'rb') as source:
    service = plistlib.load(source)
assert service['Label'] == 'git.kdnk.wm'
assert service['ProgramArguments'] == [sys.argv[2] + '/wm']
assert service['EnvironmentVariables']['WM_INSTALL_DIR'] == sys.argv[2]
assert 'RIFT_INSTALL_DIR' not in service['EnvironmentVariables']
assert service['EnvironmentVariables']['PATH'] == sys.argv[3]
assert service['StandardOutPath'] == '/tmp/wm_' + sys.argv[4] + '.out.log'
assert service['StandardErrorPath'] == '/tmp/wm_' + sys.argv[4] + '.err.log'
print('PASS: wm service uses the native executable and preserves XML metacharacters')
PY
