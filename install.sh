#!/bin/sh
# Modified by kdnk for wm on 2026-11-14. Original attribution is preserved in LICENSE.

main() {
    set -eu

    if [ "$(uname -s)" != Darwin ]; then
        printf '%s\n' 'wm installation requires macOS.' >&2
        return 1
    fi
    if ! command -v curl >/dev/null 2>&1; then
        printf '%s\n' 'curl is required to install wm.' >&2
        return 1
    fi
    if ! command -v python3 >/dev/null 2>&1 ||
        ! python3 -c 'import sys; sys.exit(sys.version_info < (3, 8))' >/dev/null 2>&1; then
        printf '%s\n' 'Python 3.8 or later is required. Install Python 3, then run this command again.' >&2
        return 1
    fi

    wm_install_tmp=$(mktemp -d "${TMPDIR:-/tmp}/wm-install.XXXXXXXX")
    trap 'rm -rf "$wm_install_tmp"' EXIT
    trap 'exit 1' HUP INT TERM
    curl --fail --location --silent --show-error --proto '=https' --proto-redir '=https' \
        --output "$wm_install_tmp/install-release.py" \
        https://raw.githubusercontent.com/kdnk/wm/main/scripts/install-release.py
    python3 "$wm_install_tmp/install-release.py" "$@"
}

main "$@"
