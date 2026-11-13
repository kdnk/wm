#!/usr/bin/env python3
# Modified by kdnk for wm on 2026-11-14. Original attribution is preserved in LICENSE.
"""Check wm command names and IPC isolation without contacting a live daemon."""

import argparse
import os
from pathlib import Path
import subprocess
import uuid


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary_dir", type=Path)
    args = parser.parse_args()
    for name in ["wm", "wm-cli"]:
        binary = args.binary_dir / name
        version = subprocess.check_output([str(binary), "--version"], text=True)
        assert version.startswith(name + " "), version
        help_text = subprocess.check_output([str(binary), "--help"], text=True)
        assert name in help_text
        assert "rift" not in help_text.lower(), help_text
    namespace = "git.kdnk.wm.test." + uuid.uuid4().hex
    result = subprocess.run([str(args.binary_dir / "wm-cli"), "query", "windows"],
                            env=dict(os.environ, WM_BS_NAME=namespace),
                            text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=15)
    assert result.returncode != 0, "An isolated nonexistent service unexpectedly answered"
    assert "not registered" in result.stderr, result.stderr
    assert "wm service start" in result.stderr, result.stderr
    print("PASS: wm and wm-cli report their names and honor an isolated WM_BS_NAME")


if __name__ == "__main__":
    main()
