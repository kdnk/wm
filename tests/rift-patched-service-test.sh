#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd -P)
source_binary="${RIFT_SERVICE_TEST_BINARY:-$root/target/debug/rift}"
if [[ ! -x "$source_binary" ]]; then
    echo "Set RIFT_SERVICE_TEST_BINARY to a built daemon, or run cargo build --bin rift." >&2
    exit 1
fi

scratch=$(mktemp -d)
scratch=$(cd "$scratch" && pwd -P)
trap 'rm -rf "$scratch"' EXIT
install_dir="$scratch/native & <cache> \"quoted\" &amp;"
fixture_user='fixture & <user> &amp;'
fixture_path="$install_dir:$scratch/tools & <utilities> &amp;/bin:$PATH"
mkdir -p "$install_dir"
cp "$source_binary" "$install_dir/rift"
cp "$source_binary" "$install_dir/rift-patched"
for name in rift rift-patched; do
fixture_home="$scratch/home-$name"
mkdir -p "$fixture_home"
HOME="$fixture_home" USER="$fixture_user" PATH="$fixture_path" \
    "$install_dir/$name" service install

plist="$fixture_home/Library/LaunchAgents/git.acsandmann.rift.plist"
/usr/bin/plutil -lint "$plist"
python3 - "$plist" "$install_dir" "$fixture_path" "$fixture_user" "$name" <<'PY'
import plistlib
import sys

with open(sys.argv[1], 'rb') as source:
    service = plistlib.load(source)
assert service['ProgramArguments'] == [sys.argv[2] + '/' + sys.argv[5]]
assert service['EnvironmentVariables']['RIFT_INSTALL_DIR'] == sys.argv[2]
assert service['EnvironmentVariables']['PATH'] == sys.argv[3]
assert service['StandardOutPath'] == '/tmp/rift_' + sys.argv[4] + '.out.log'
assert service['StandardErrorPath'] == '/tmp/rift_' + sys.argv[4] + '.err.log'
print('PASS: LaunchAgent preserves XML metacharacters in paths and environment values')
PY
done
