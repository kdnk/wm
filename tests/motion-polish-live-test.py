#!/usr/bin/env python3
# Modified by kdnk for wm on 2026-11-14. Original attribution is preserved in LICENSE.
"""Record motion and pointer behavior using PID-checked disposable windows."""

import argparse
import ctypes
import importlib.util
import json
import os
from pathlib import Path
import select
import subprocess
import time
import uuid


class Point(ctypes.Structure):
    _fields_ = [("x", ctypes.c_double), ("y", ctypes.c_double)]


class Pointer:
    def __init__(self):
        self.cg = ctypes.CDLL("/System/Library/Frameworks/CoreGraphics.framework/CoreGraphics")
        self.cf = ctypes.CDLL("/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation")
        self.cg.CGEventCreate.argtypes = [ctypes.c_void_p]
        self.cg.CGEventCreate.restype = ctypes.c_void_p
        self.cg.CGEventGetLocation.argtypes = [ctypes.c_void_p]
        self.cg.CGEventGetLocation.restype = Point
        self.cg.CGWarpMouseCursorPosition.argtypes = [Point]
        self.cg.CGEventCreateMouseEvent.argtypes = [ctypes.c_void_p, ctypes.c_uint, Point, ctypes.c_uint]
        self.cg.CGEventCreateMouseEvent.restype = ctypes.c_void_p
        self.cg.CGEventSetFlags.argtypes = [ctypes.c_void_p, ctypes.c_uint64]
        self.cg.CGEventPost.argtypes = [ctypes.c_uint, ctypes.c_void_p]
        self.cf.CFRelease.argtypes = [ctypes.c_void_p]

    def position(self):
        event = self.cg.CGEventCreate(None)
        try:
            point = self.cg.CGEventGetLocation(event)
            return [point.x, point.y]
        finally:
            self.cf.CFRelease(event)

    def warp(self, point):
        self.cg.CGWarpMouseCursorPosition(Point(*point))

    def mouse(self, kind, point, flags=0):
        event = self.cg.CGEventCreateMouseEvent(None, kind, Point(*point), 0)
        try:
            self.cg.CGEventSetFlags(event, flags)
            self.cg.CGEventPost(0, event)
        finally:
            self.cf.CFRelease(event)


def run(args):
    helper = Path(__file__).with_name("rift-settings-focus-live-test.py")
    spec = importlib.util.spec_from_file_location("wm_motion_focus", helper)
    focus_check = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(focus_check)
    focus_check.require_unlocked()
    args.output.mkdir(parents=True)
    client = str(args.client.resolve())
    pointer = Pointer()

    def query(kind):
        return json.loads(subprocess.check_output([client, "query", kind], timeout=5))

    def execute(*command):
        focus_check.require_unlocked()
        subprocess.run([client, "execute", *command], check=True, capture_output=True, timeout=5)

    def setting(key, value):
        execute("config", "set", key, json.dumps(value))

    config = json.loads(subprocess.check_output([client, "execute", "config", "get"], timeout=5))
    overrides = {
        "settings.animate": True,
        "settings.mouse_hides_on_focus": False,
        "settings.focus_follows_mouse": False,
        "settings.layout.scrolling.preserve_window_sizes": False,
        "settings.layout.scrolling.column_width_ratio": 0.55,
        "settings.layout.scrolling.preset_column_widths": [0.5, 1.0],
        "settings.drag_drop.modifier": "alt",
    }
    original_settings = {}
    for key in overrides:
        value = config
        for part in key.split("."):
            value = value[part]
        original_settings[key] = value
    original_windows = query("windows")
    original_focus = next((w for w in original_windows if w["is_focused"]), None)
    deadline = time.monotonic() + 10
    active = None
    while active is None:
        active = next((w for w in query("workspaces") if w["is_focused"]), None)
        if active is None:
            assert time.monotonic() < deadline, "wm has not published an active workspace"
            time.sleep(0.1)
    displays = query("displays")
    display_index, display = next((index + 1, display) for index, display in enumerate(displays)
                                  if display["uuid"] == active["display_uuid"])
    screen_left = display["frame"]["origin"]["x"]
    screen_right = screen_left + display["frame"]["size"]["width"]
    empty = next((w for w in query("workspaces") if w["display_uuid"] == active["display_uuid"]
                  and w["window_count"] == 0 and w["layout_mode"] == "scrolling"), None)
    if empty is None:
        raise RuntimeError("An empty scrolling workspace is required")
    original_pointer = pointer.position()
    report = {"label": "wm-polish-" + uuid.uuid4().hex[:8], "scenes": [], "failures": []}
    fixture = None

    def focus(window):
        assert window["id"]["pid"] == fixture.pid, "Refusing to focus a non-fixture window"
        execute("window", "focus", "--window-id", json.dumps(window["id"]),
                "--window-server-id", str(window["window_server_id"]))

    def reply():
        if not select.select([fixture.stdout], [], [], 5)[0]:
            raise AssertionError("Fixture did not respond")
        line = fixture.stdout.readline()
        if not line:
            raise AssertionError(f"Fixture exited with {fixture.poll()}")
        return json.loads(line)

    def command(operation):
        fixture.stdin.write(json.dumps({"op": operation}) + "\n")
        fixture.stdin.flush()
        return reply()

    def windows():
        return sorted((w for w in query("windows") if w["id"]["pid"] == fixture.pid),
                      key=lambda w: w["title"])

    def scene(name, action):
        recorder = subprocess.Popen([
            "/usr/sbin/screencapture", "-x", "-v", "-V", "3", "-D", str(display_index),
            str(args.output / (name + ".mov")),
        ], stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        try:
            time.sleep(0.3)
            command("start")
            action()
            time.sleep(1)
            samples = command("trace")
        finally:
            _, error = recorder.communicate(timeout=8)
        assert recorder.returncode == 0, error.decode()
        current = windows()
        record = {"name": name, "samples": samples, "windows": current,
                  "pointer": pointer.position(), "video": name + ".mov"}
        report["scenes"].append(record)
        native = [sample for sample in samples if sample["kind"] == "sample"]
        assert native, "No native samples captured"
        for window in current:
            frame = window["frame"]
            expected = [frame["origin"]["x"], frame["origin"]["y"],
                        frame["size"]["width"], frame["size"]["height"]]
            actual = native[-1]["frames"][window["title"]]
            assert all(abs(a - b) <= 1 for a, b in zip(actual, expected)), (
                f"{name}: native geometry {actual} differs from {expected}")
        print("PASS native geometry", name, flush=True)

    try:
        for key, value in overrides.items():
            setting(key, value)
        execute("workspace", "switch", str(empty["index"]))
        fixture = subprocess.Popen([
            str(args.qt_python), "-u", str(Path(__file__).with_name("rift-content-render-live-test.py")),
            "--fixture", report["label"],
        ], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True,
            env=dict(os.environ, PYTHONPATH=str(args.qt_packages), WM_FIXTURE_WINDOW_COUNT="4"))
        assert reply()["pid"] == fixture.pid
        deadline = time.monotonic() + 10
        while len(windows()) != 4:
            assert time.monotonic() < deadline, "wm did not manage fixture windows"
            time.sleep(0.1)
        targets = sorted(windows(), key=lambda w: w["layout_position"]["column"])

        def prepare(index):
            focus(targets[index])
            time.sleep(0.7)

        prepare(0)
        scene("short-pan", lambda: focus(targets[1]))
        prepare(0)
        scene("long-pan", lambda: focus(targets[3]))
        prepare(0)

        def repeat():
            for _ in range(3):
                execute("window", "focus", "right")
                time.sleep(0.045)
        scene("repeat", repeat)
        prepare(0)

        def reverse():
            focus(targets[3])
            time.sleep(0.045)
            focus(targets[0])
        scene("reverse", reverse)
        prepare(1)
        scene("resize", lambda: execute("layout", "switch-preset-column-width"))
        execute("layout", "switch-preset-column-width")
        time.sleep(0.4)
        prepare(0)
        frame = next(w for w in windows() if w["id"] == targets[1]["id"])["frame"]
        inside = [(max(screen_left, frame["origin"]["x"]) + min(screen_right, frame["origin"]["x"] + frame["size"]["width"])) / 2,
                  frame["origin"]["y"] + frame["size"]["height"] * 0.4]
        pointer.warp(inside)
        before = pointer.position()
        focus(targets[1])
        time.sleep(0.2)
        retained = pointer.position()
        report["cursor_retention"] = {"before": before, "after": retained}
        if args.expect_cursor_retention:
            assert max(abs(a - b) for a, b in zip(before, retained)) <= 1, "Cursor moved inside focused window"
        prepare(1)
        before_drop = {w["title"]: w["layout_position"] for w in windows()}

        def drag():
            by_id = {json.dumps(w["id"], sort_keys=True): w for w in windows()}
            source = by_id[json.dumps(targets[1]["id"], sort_keys=True)]
            target = by_id[json.dumps(targets[0]["id"], sort_keys=True)]
            a, b = source["frame"], target["frame"]
            def visible_center(frame):
                left = max(screen_left, frame["origin"]["x"])
                right = min(screen_right, frame["origin"]["x"] + frame["size"]["width"])
                assert right - left > 60, "Drag participants must both be visible"
                return [(left + right) / 2, frame["origin"]["y"] + frame["size"]["height"] * 0.5]
            start, end = visible_center(a), visible_center(b)
            pointer.mouse(1, start, 1 << 19)
            for index in range(1, 21):
                t = index / 20
                pointer.mouse(6, [a + (b - a) * t for a, b in zip(start, end)], 1 << 19)
                time.sleep(0.025)
            pointer.mouse(6, [screen_left + 15, display["frame"]["origin"]["y"] - 10], 1 << 19)
            time.sleep(0.035)
            pointer.mouse(6, end, 1 << 19)
            time.sleep(0.06)
            pointer.mouse(2, end, 1 << 19)
        scene("drop", drag)
        assert before_drop != {w["title"]: w["layout_position"] for w in windows()}, "Modifier drag did not change the layout"
        prepare(3)
        focused = next(w for w in query("windows") if w["is_focused"])
        assert focused["id"]["pid"] == fixture.pid and focused["title"] == targets[3]["title"]
        scene("close-gap", lambda: subprocess.run([str(args.closer.resolve()), str(fixture.pid),
                                                  targets[3]["title"]], check=True,
                                                  capture_output=True, timeout=5))
        assert len(windows()) == 3, "The disposable native close did not remove one fixture window"
    except Exception as error:
        report["failures"].append(str(error))
        raise
    finally:
        (args.output / "report.json").write_text(json.dumps(report, indent=2) + "\n")
        try:
            if fixture and fixture.poll() is None:
                fixture.stdin.write('{"op":"quit"}\n')
                fixture.stdin.flush()
                try:
                    fixture.wait(timeout=3)
                except subprocess.TimeoutExpired:
                    fixture.terminate()
                    fixture.wait(timeout=3)
        finally:
            for key, value in original_settings.items():
                setting(key, value)
            execute("workspace", "switch", str(active["index"]))
            if original_focus:
                execute("window", "focus", "--window-id", json.dumps(original_focus["id"]),
                        "--window-server-id", str(original_focus["window_server_id"]))
            pointer.warp(original_pointer)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--client", type=Path, default=Path("target/debug/wm-cli"))
    parser.add_argument("--closer", type=Path, default=Path("target/debug/examples/close-window-check"))
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--qt-python", type=Path, default=Path("/Applications/Anki.app/Contents/Frameworks/Python.framework/Versions/3.13/bin/python3.13"))
    parser.add_argument("--qt-packages", type=Path, default=Path("/Applications/Anki.app/Contents/Resources/app_packages"))
    parser.add_argument("--expect-cursor-retention", action="store_true")
    run(parser.parse_args())
