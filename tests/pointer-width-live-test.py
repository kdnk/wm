#!/usr/bin/env python3
# Modified by kdnk for wm on 2026-11-17. Original attribution is preserved in LICENSE.
"""Check pointer-selected resize edges against a matching running wm daemon."""

import argparse
import importlib.util
import json
import os
from pathlib import Path
import select
import subprocess
import sys
import time
import uuid

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[1]


def helper(name, filename):
    spec = importlib.util.spec_from_file_location(name, ROOT / "tests" / filename)
    loaded = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(loaded)
    return loaded


def run(args):
    live = helper("wm_pointer_live", "rift-live-resize-live-test.py")
    motion = helper("wm_pointer_motion", "motion-polish-live-test.py")
    safety = helper("wm_pointer_safety", "rift-settings-focus-live-test.py")
    safety.require_unlocked()
    client = str(args.client.expanduser().resolve())

    def query(kind):
        return json.loads(subprocess.check_output([client, "query", kind], timeout=5))

    def execute(*command):
        safety.require_unlocked()
        subprocess.run([client, "execute", *command], check=True, capture_output=True, timeout=5)

    def setting(key, value):
        execute("config", "set", key, json.dumps(value))

    before = query("windows")
    active = next(w for w in query("workspaces") if w["is_focused"])
    empty = next((w for w in query("workspaces") if w["display_uuid"] == active["display_uuid"]
                  and w["window_count"] == 0 and w["layout_mode"] == "scrolling"), None)
    assert empty, "An empty scrolling workspace is required"
    display = next(d for d in query("displays") if d["uuid"] == active["display_uuid"])
    if not args.run:
        print(json.dumps({"ready": True, "mutations": False, "workspace": empty["index"],
                          "display": display["uuid"]}, indent=2))
        return
    args.output.mkdir(parents=True, exist_ok=False)
    config = json.loads(subprocess.check_output([client, "execute", "config", "get"], timeout=5))
    overrides = {
        "settings.animate": True,
        "settings.animation_duration": 0.25,
        "settings.layout.scrolling.animate": True,
        "settings.mouse_follows_focus": False,
        "settings.mouse_hides_on_focus": False,
        "settings.focus_follows_mouse": False,
        "settings.layout.scrolling.preserve_window_sizes": False,
        "settings.layout.scrolling.expand_single_column": False,
        "settings.layout.scrolling.preset_column_widths": [1.0],
    }
    original_settings = {}
    for key in overrides:
        value = config
        for part in key.split("."):
            value = value[part]
        original_settings[key] = value
    pointer = motion.Pointer()
    original_pointer = pointer.position()
    original_focus = next((w for w in before if w["is_focused"]), None)
    backup = (args.output / "before-layout.ron").resolve()
    execute("save-layout", str(backup))
    label = "wm-pointer-width-" + uuid.uuid4().hex[:8]
    title = label + "-0"
    fixture = native = recorder = None
    trace_start = args.trace_file.stat().st_size if args.trace_file and args.trace_file.exists() else 0
    report = {"label": label, "before_windows": before, "original_settings": original_settings,
              "original_pointer": original_pointer, "original_workspace": active["index"],
              "scenes": [], "failures": []}

    def reply():
        assert select.select([fixture.stdout], [], [], 6)[0], "Fixture response timed out"
        line = fixture.stdout.readline()
        assert line, f"Fixture exited with {fixture.poll()}"
        return json.loads(line)

    def fixture_command(operation):
        fixture.stdin.write(json.dumps({"op": operation}) + "\n")
        fixture.stdin.flush()
        return reply()

    def target():
        window = next(w for w in query("windows") if w["id"]["pid"] == fixture.pid
                      and w["title"] == title)
        assert not window["is_floating"] and window["layout_position"] is not None
        current = next(w for w in query("workspaces") if w["is_focused"])
        assert current["index"] == empty["index"], "Focus left the verification workspace"
        return window

    def move(side):
        frame = display["frame"]
        pointer.warp([frame["origin"]["x"] + frame["size"]["width"] * (0.25 if side == "left" else 0.75),
                      frame["origin"]["y"] + frame["size"]["height"] * 0.5])

    def change(name, command, side, anchor=True):
        window = target()
        move(side)
        execute("window", "focus", "--window-id", json.dumps(window["id"]),
                "--window-server-id", str(window["window_server_id"]))
        time.sleep(0.15)
        window = target()
        report["last_focus"] = {"managed": window["is_focused"], "frontmost_pid": live.frontmost_pid(),
                                "native_window": native.focused_window_id(), "fixture_pid": fixture.pid}
        assert window["is_focused"] and report["last_focus"]["frontmost_pid"] == fixture.pid, "Focus left fixture"
        assert report["last_focus"]["native_window"] == window["window_server_id"], "Native focus left fixture"
        start = native.frame()
        execute(*command)
        time.sleep(args.settle)
        final = native.frame()
        managed = target()
        scene = {"name": name, "side": side, "from": start, "final": final,
                 "command": command, "managed": managed}
        report["scenes"].append(scene)
        assert live.near(final, live.rect(managed["frame"])), f"{name}: native and target differ"
        assert abs(final[2] - start[2]) > 5, f"{name}: width did not change"
        if anchor:
            edge = lambda frame: frame[0] + (frame[2] if side == "right" else 0)
            assert abs(edge(final) - edge(start)) <= 2, f"{name}: {side} edge moved: {start} -> {final}"
        return final

    try:
        for key, value in overrides.items():
            setting(key, value)
        execute("workspace", "switch", str(empty["index"]))
        fixture = subprocess.Popen(
            [str(args.qt_python), "-u", str(ROOT / "tests/rift-live-resize-live-test.py"), "--fixture", label],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=(args.output / "fixture.stderr").open("w"), text=True,
            env=dict(os.environ, PYTHONPATH=str(args.qt_packages), WM_FIXTURE_WINDOW_COUNT="1",
                     PYTHONDONTWRITEBYTECODE="1"))
        ready = reply()
        assert ready["pid"] == fixture.pid, "Fixture PID differs"
        deadline = time.monotonic() + 15
        while not any(w["id"]["pid"] == fixture.pid and w["title"] == title for w in query("windows")):
            assert time.monotonic() < deadline, "wm did not manage fixture"
            time.sleep(0.1)
        window = next(w for w in query("windows") if w["id"]["pid"] == fixture.pid
                      and w["title"] == title)
        report["target"] = window
        execute("window", "focus", "--window-id", json.dumps(window["id"]),
                "--window-server-id", str(window["window_server_id"]))
        time.sleep(0.2)
        current = next(w for w in query("workspaces") if w["is_focused"])
        if current["index"] != empty["index"]:
            selected = next(w for w in query("windows") if w["is_focused"])
            assert selected["id"] == window["id"], "Fixture must be selected before moving"
            execute("workspace", "move-window", str(empty["index"]), "--follow",
                    str(window["window_server_id"]))
            time.sleep(0.2)
        target()
        native = live.NativeWindow(fixture.pid, window["window_server_id"])
        time.sleep(args.settle)
        move("left")
        execute("layout", "switch-preset-column-width")
        time.sleep(args.settle)
        setting("settings.layout.scrolling.preset_column_widths", [0.5, 1.0])
        fixture_command("start")
        if args.video:
            capture = ",".join(str(round(v)) for v in live.rect(display["frame"]))
            recorder = subprocess.Popen(["/usr/sbin/screencapture", "-x", "-v", "-V", "20", "-R", capture,
                                         str(args.output / "width-changes.mov")],
                                        stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
            time.sleep(0.4)
        preset = ["layout", "switch-preset-column-width"]
        full = native.frame()
        for side in ["right", "left"]:
            half = change(f"{side}-full-to-half", preset, side)
            assert half[2] < full[2] * 0.6, "Full-to-half setup did not shrink"
            returned = change(f"{side}-to-full", preset, side, anchor=False)
            assert live.near(returned, full), "Full width did not return to its original frame"
        setting("settings.layout.scrolling.preset_column_widths", [0.6, 0.4])
        change("inset-60-setup", preset, "left")
        execute("layout", "center-selection")
        time.sleep(args.settle)
        inset = native.frame()
        assert inset[0] > full[0] + 20 and sum(inset[::2]) < sum(full[::2]) - 20, "Inset setup failed"
        change("inset-60-to-40-right", preset, "right")
        change("inset-40-to-60-right", ["window", "resize-by", "--amount=0.2"], "right")
        change("inset-60-to-40-left", ["window", "resize-by", "--amount=-0.2"], "left")
        report["fixture_trace"] = fixture_command("trace")
        if args.trace_file:
            report["daemon_trace"] = str(args.trace_file.resolve())
            trace = args.trace_file.read_bytes()[trace_start:].decode(errors="replace")
            report["daemon_trace_excerpt"] = trace
            assert "SwitchPresetColumnWidth" in trace and "ResizeWindowBy" in trace, "Daemon log has no width commands from this run"
    except Exception as error:
        report["failures"].append(str(error))
        raise
    finally:
        errors = []

        def cleanup(action):
            try:
                action()
            except Exception as error:
                errors.append(str(error))

        def stop_fixture():
            if fixture and fixture.poll() is None:
                try:
                    fixture.stdin.write('{"op":"quit"}\n')
                    fixture.stdin.flush()
                    fixture.wait(timeout=5)
                finally:
                    if fixture.poll() is None:
                        fixture.kill()
                        fixture.wait(timeout=5)

        try:
            if recorder:
                try:
                    _, stderr = recorder.communicate(timeout=35)
                except subprocess.TimeoutExpired:
                    recorder.kill()
                    recorder.communicate(timeout=5)
                    raise RuntimeError("Display recorder did not finish")
                report["video_stderr"] = stderr.decode(errors="replace")
                video = args.output / "width-changes.mov"
                assert recorder.returncode == 0 and video.is_file() and video.stat().st_size > 0, "Display recording failed"
                subprocess.run(["/usr/bin/avconvert", "--source", str(video), "--preset", "Preset640x480",
                                "--duration", "0.25", "--output", str(args.output / "video-read-check.mov")],
                               check=True, capture_output=True, timeout=12)
        except Exception as error:
            errors.append(str(error))
        cleanup(stop_fixture)
        if native:
            cleanup(native.dispose)
        for key, value in original_settings.items():
            cleanup(lambda: setting(key, value))
        cleanup(lambda: execute("workspace", "switch", str(active["index"])))
        cleanup(lambda: execute("load-layout", str(backup), "--scope", "displays"))
        if original_focus:
            cleanup(lambda: execute("window", "focus", "--window-id", json.dumps(original_focus["id"]),
                                    "--window-server-id", str(original_focus["window_server_id"])))
        try:
            deadline = time.monotonic() + 8
            while True:
                restored = {w["window_server_id"]: w for w in query("windows")}
                report["layout_restored"] = all(
                    (actual := restored.get(w["window_server_id"])) is not None
                    and actual["layout_position"] == w["layout_position"]
                    and all(abs(actual["frame"]["size"][axis] - w["frame"]["size"][axis]) <= 2
                            for axis in ["width", "height"]) for w in before)
                if report["layout_restored"] or time.monotonic() >= deadline:
                    break
                time.sleep(0.1)
            assert report["layout_restored"], "Original window order or sizes were not restored"
        except Exception as error:
            errors.append(str(error))
        finally:
            cleanup(lambda: pointer.warp(original_pointer))
            report["cleanup_errors"] = errors
            (args.output / "report.json").write_text(json.dumps(report, indent=2) + "\n")
        if errors:
            raise RuntimeError("; ".join(errors))
    print(f"PASS pointer-selected native edges in {len(report['scenes'])} width changes; layout restored", flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--client", type=Path, default=Path("~/.local/bin/wm"))
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--run", action="store_true")
    parser.add_argument("--settle", type=float, default=1)
    parser.add_argument("--video", action="store_true")
    parser.add_argument("--trace-file", type=Path, help="The matching daemon's stderr log with RUST_LOG=info.")
    parser.add_argument("--qt-python", type=Path, default=Path("/Applications/Anki.app/Contents/Frameworks/Python.framework/Versions/3.13/bin/python3.13"))
    parser.add_argument("--qt-packages", type=Path, default=Path("/Applications/Anki.app/Contents/Resources/app_packages"))
    options = parser.parse_args()
    if options.video and not options.trace_file:
        parser.error("--video requires --trace-file from the matching daemon")
    run(options)
