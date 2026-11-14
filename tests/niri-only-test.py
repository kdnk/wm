#!/usr/bin/env python3
# Modified by kdnk for wm on 2026-11-14. Original attribution is preserved in LICENSE.
"""Check the scrolling-only CLI and configuration without a running daemon."""

import argparse
import os
from pathlib import Path
import subprocess
import tempfile
import uuid


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary_dir", type=Path)
    args = parser.parse_args()
    binaries = args.binary_dir.resolve()
    env = dict(os.environ, WM_BS_NAME="git.kdnk.wm.test." + uuid.uuid4().hex)

    def run(binary, *arguments):
        return subprocess.run([str(binary), *arguments], env=env, text=True,
                              capture_output=True, timeout=15)

    client = binaries / "wm"
    for group, command in [
        ("workspace", "set-layout"),
        ("layout", "toggle-orientation"),
        ("layout", "balance"),
        ("layout", "adjust-master-ratio"),
        ("layout", "adjust-master-count"),
        ("layout", "promote-to-master"),
        ("layout", "swap-master-stack"),
        ("layout", "toggle-focus-float"),
        ("window", "toggle-float"),
        ("config", "set-stack-offset"),
        ("config", "set-stack-default-orientation"),
    ]:
        result = run(client, "execute", group, command, "--help")
        assert result.returncode == 2, (group, command, result)
        assert "unrecognized subcommand" in result.stderr, result.stderr

    for group, command in [
        ("layout", "join-window"),
        ("layout", "toggle-stack"),
        ("layout", "consume-or-expel-window"),
        ("layout", "switch-preset-column-width"),
        ("layout", "center-selection"),
        ("workspace", "move-window"),
    ]:
        result = run(client, "execute", group, command, "--help")
        assert result.returncode == 0, (group, command, result)

    template = (Path(__file__).resolve().parents[1] / "wm.default.toml").read_text()
    checker = binaries / "examples" / "config-check"
    with tempfile.TemporaryDirectory(prefix="wm-niri-") as directory:
        config = Path(directory) / "config.toml"

        def check(text, valid):
            config.write_text(text)
            result = run(checker, str(config))
            assert (result.returncode == 0) == valid, result

        check(template, True)
        check(template.replace('mode = "scrolling"\n', "", 1), True)
        for mode in ["traditional", "bsp", "stack", "master_stack", "floating"]:
            check(template.replace('mode = "scrolling"', f'mode = "{mode}"', 1), False)
        for section in ["traditional", "bsp", "stack", "master_stack"]:
            check(template + f"\n[settings.layout.{section}]\n", False)
        check(template.replace("[settings.layout.scrolling]\n",
                               '[settings.layout.scrolling]\nfocus_navigation_style = "anchored"\n'), False)
        check(template.replace("app_rules = []", 'app_rules = [{ app_id = "example.app", floating = false }]'), True)
        for rule in ["floating = true", "position = { x = 0.5, y = 0.5 }"]:
            check(template.replace("app_rules = []", f'app_rules = [{{ app_id = "example.app", {rule} }}]'), False)
        for command in ["toggle_window_floating", "toggle_focus_floating", "balance",
                        "toggle_orientation", "promote_to_master", "swap_master_stack"]:
            check(template.replace("[keys]\n", f'[keys]\n"Ctrl + Alt + X" = "{command}"\n'), False)
    print("PASS: scrolling commands remain available and removed layouts, floating controls, and anchored settings are rejected")


if __name__ == "__main__":
    main()
