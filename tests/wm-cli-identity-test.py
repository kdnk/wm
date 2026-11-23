#!/usr/bin/env python3
# Modified by kdnk for wm on 2026-11-14. Original attribution is preserved in LICENSE.
"""Check unified wm commands and IPC isolation without contacting a live daemon."""

import argparse
import os
from pathlib import Path
import re
import subprocess
import tempfile
import uuid


def run(binary, arguments, environment):
    return subprocess.run([str(binary), *arguments], env=environment, text=True,
                          capture_output=True, timeout=15)


def command_help(binary, arguments, environment):
    result = run(binary, [*arguments, "--help"], environment)
    assert result.returncode == 0, result
    return result.stdout


def check_config_file(binary, environment):
    template = Path(__file__).resolve().parents[1] / "wm.default.toml"
    with tempfile.TemporaryDirectory(prefix="wm-config-check-") as directory:
        home = Path(directory)
        env = dict(environment, HOME=str(home))
        config = home / ".config" / "wm" / "config.toml"
        missing = run(binary, ["config", "check"], env)
        assert missing.returncode == 1 and str(config) in missing.stderr, missing
        assert not config.exists(), config

        config.parent.mkdir(parents=True)
        config.write_bytes(template.read_bytes())
        result = run(binary, ["config", "check"], env)
        assert result.returncode == 0 and str(config) in result.stdout, result
        assert config.read_bytes() == template.read_bytes(), config

        draft = home / "config draft.toml"
        draft.write_bytes(template.read_bytes())
        result = run(binary, ["config", "check", str(draft)], env)
        assert result.returncode == 0 and str(draft) in result.stdout, result
        for content, diagnostic in [
            ("[settings\n", "TOML parse error"),
            ("[settings]\n[virtual_workspaces]\ndefault_workspace_count = 0\n[keys]\n",
             "default_workspace_count must be at least 1"),
        ]:
            draft.write_text(content)
            result = run(binary, ["config", "check", str(draft)], env)
            assert result.returncode == 1 and not result.stdout, result
            assert str(draft) in result.stderr and diagnostic in result.stderr, result
            assert draft.read_text() == content, draft
    print("PASS: wm config check validates default and explicit paths without a daemon")


def check_config_command_help(binary, environment):
    command = ["execute", "config", "set"]
    help_text = command_help(binary, command, environment)
    example = re.search(r"wm execute config set ([^\n]+)", help_text)
    assert example, help_text
    result = run(binary, [*command, *example[1].split()], environment)
    assert result.returncode == 1 and "not registered" in result.stderr, result

    command = ["execute", "config", "set-animation-easing"]
    assert "ease_in_out_sine" in command_help(binary, command, environment)
    invalid = run(binary, [*command, "ease-in-sine"], environment)
    assert invalid.returncode == 1, invalid
    assert all(name in invalid.stderr for name in ["ease-in-sine", "ease_in_sine", "linear"]), invalid
    valid = run(binary, [*command, "ease_in_sine"], environment)
    assert valid.returncode == 1 and "not registered" in valid.stderr, valid
    print("PASS: config help shows usable syntax and easing errors list accepted values")


def check_client_help(binary, baseline, environment):
    pending = [(name,) for name in ("query", "execute", "subscribe")]
    checked = 0
    while pending:
        arguments = pending.pop()
        before = command_help(baseline, arguments, environment).replace("wm-cli", "wm")
        if arguments == ("subscribe",):
            before = re.sub(r"^  unsub-mach\s.*\n", "", before, flags=re.M)
        if arguments in [("subscribe",), ("subscribe", "mach")]:
            before = before.replace(
                "Subscribe to Mach IPC events",
                "Print Mach IPC events until this process exits. Press Ctrl-C to unsubscribe",
            )
        if arguments in [("execute", "config"), ("execute", "config", "set")]:
            summary = "Set a configuration value by its dot-separated key"
            replacement = summary if arguments[-1] == "config" else (
                summary + "\n\nExample: wm execute config set settings.animate true"
            )
            before = before.replace(
                "Generic set: set an arbitrary config key (dot-separated path) to a JSON value. "
                "Example: wm execute config set --key settings.animate --value true",
                replacement,
            )
            if arguments[-1] == "set":
                before = before.replace("Print help\n", "Print help (see a summary with '-h')\n")
        if arguments == ("execute", "config", "set-animation-easing"):
            before = before.replace(
                "Arguments:\n  <VALUE>",
                "Arguments:\n  <VALUE>  Curve name, such as linear, ease_in_out, or ease_in_out_sine",
            )
        after = command_help(binary, arguments, environment)
        assert before.split() == after.split(), (arguments, before, after)
        checked += 1
        commands = re.search(r"^Commands:\n(.*?)(?=^\S|\Z)", before, re.M | re.S)
        if commands:
            for name in re.findall(r"^  ([a-z][a-z0-9-]*)\s", commands[1], re.M):
                if name != "help":
                    pending.append((*arguments, name))
    print(f"PASS: {checked} client help pages match the original CLI with the expected updates")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary_dir", type=Path)
    parser.add_argument("--baseline-client", type=Path,
                        help="compare retained client commands with an original wm-cli build")
    args = parser.parse_args()
    binary = args.binary_dir.resolve() / "wm"
    environment = dict(os.environ, WM_BS_NAME="git.kdnk.wm.test." + uuid.uuid4().hex)
    version = run(binary, ["--version"], environment)
    assert version.returncode == 0 and version.stdout.startswith("wm "), version
    help_text = command_help(binary, [], environment)
    assert "rift" not in help_text.lower(), help_text
    check_config_file(binary, environment)
    check_config_command_help(binary, environment)
    for command in ["query", "execute", "subscribe", "service", "close-window", "config"]:
        assert re.search(r"^  " + command + r"\s", help_text, re.M), help_text
        command_help(binary, [command], environment)
    close_help = command_help(binary, ["close-window"], environment)
    assert "native close button" in close_help, close_help
    subscribe_help = command_help(binary, ["subscribe"], environment)
    assert "unsub-mach" not in subscribe_help, subscribe_help
    result = run(binary, ["subscribe", "unsub-mach", "windows_changed"], environment)
    assert result.returncode == 2, result
    assert "unrecognized subcommand" in result.stderr, result.stderr
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
        assert environment["WM_BS_NAME"] in result.stderr, result.stderr
        assert "wm service start" not in result.stderr, result.stderr
    result = run(binary, ["subscribe", "mach", "windows_chagned"], environment)
    assert result.returncode == 1, result
    assert "Invalid event" in result.stderr and "windows_changed" in result.stderr, result.stderr
    assert "Communication error" not in result.stderr and "Hint:" not in result.stderr, result.stderr
    if args.baseline_client:
        check_client_help(binary, args.baseline_client.resolve(), environment)
    print("PASS: unified wm exposes daemon and client commands and honors isolated WM_BS_NAME")


if __name__ == "__main__":
    main()
