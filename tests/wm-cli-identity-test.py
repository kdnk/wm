#!/usr/bin/env python3
# Modified by kdnk for wm on 2026-11-14. Original attribution is preserved in LICENSE.
"""Check unified wm commands and IPC isolation without contacting a live daemon."""

import argparse
import os
from pathlib import Path
import re
import subprocess
import uuid


def run(binary, arguments, environment):
    return subprocess.run([str(binary), *arguments], env=environment, text=True,
                          capture_output=True, timeout=15)


def command_help(binary, arguments, environment):
    result = run(binary, [*arguments, "--help"], environment)
    assert result.returncode == 0, result
    return result.stdout


def check_client_help(binary, baseline, environment):
    pending = [(name,) for name in ("query", "execute", "subscribe")]
    checked = 0
    while pending:
        arguments = pending.pop()
        before = command_help(baseline, arguments, environment).replace("wm-cli", "wm")
        after = command_help(binary, arguments, environment)
        assert before.split() == after.split(), (arguments, before, after)
        checked += 1
        commands = re.search(r"^Commands:\n(.*?)(?=^\S|\Z)", before, re.M | re.S)
        if commands:
            for name in re.findall(r"^  ([a-z][a-z0-9-]*)\s", commands[1], re.M):
                if name != "help":
                    pending.append((*arguments, name))
    print(f"PASS: {checked} client command help pages match the original CLI")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary_dir", type=Path)
    parser.add_argument("--baseline-client", type=Path,
                        help="compare every client command's help with an original wm-cli build")
    args = parser.parse_args()
    binary = args.binary_dir.resolve() / "wm"
    environment = dict(os.environ, WM_BS_NAME="git.kdnk.wm.test." + uuid.uuid4().hex)
    version = run(binary, ["--version"], environment)
    assert version.returncode == 0 and version.stdout.startswith("wm "), version
    help_text = command_help(binary, [], environment)
    assert "rift" not in help_text.lower(), help_text
    for command in ["query", "execute", "subscribe", "service", "close-window"]:
        assert re.search(r"^  " + command + r"\s", help_text, re.M), help_text
        command_help(binary, [command], environment)
    close_help = command_help(binary, ["close-window"], environment)
    assert "native close button" in close_help, close_help
    for argument in ["--settings", "settings"]:
        result = run(binary, [argument], environment)
        assert result.returncode == 2, result
        assert argument in result.stderr, result.stderr
        assert "unexpected argument" in result.stderr or "unrecognized subcommand" in result.stderr, result.stderr
    for arguments in [["query", "windows"], ["execute", "window", "next"],
                      ["subscribe", "mach", "windows_changed"]]:
        result = run(binary, arguments, environment)
        assert result.returncode != 0, "An isolated nonexistent service unexpectedly answered"
        assert "not registered" in result.stderr, result.stderr
        assert "wm service start" in result.stderr, result.stderr
    if args.baseline_client:
        check_client_help(binary, args.baseline_client.resolve(), environment)
    print("PASS: unified wm exposes daemon and client commands and honors isolated WM_BS_NAME")


if __name__ == "__main__":
    main()
