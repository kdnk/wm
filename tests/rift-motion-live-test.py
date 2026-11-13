#!/usr/bin/env python3
import argparse
import ctypes
import importlib.util
import json
import os
from pathlib import Path
import select
import subprocess
import sys
import time
import uuid


def fixture(label):
    from PyQt6.QtCore import QTimer, Qt
    from PyQt6.QtWidgets import QApplication, QMainWindow

    app = QApplication([])
    app.setApplicationName("Rift motion fixture")
    app.setQuitOnLastWindowClosed(False)
    objc = ctypes.CDLL("/usr/lib/libobjc.A.dylib")
    objc.sel_registerName.argtypes = [ctypes.c_char_p]
    objc.sel_registerName.restype = ctypes.c_void_p
    objc.objc_getClass.argtypes = [ctypes.c_char_p]
    objc.objc_getClass.restype = ctypes.c_void_p

    class Point(ctypes.Structure):
        _fields_ = [("x", ctypes.c_double), ("y", ctypes.c_double)]

    class Size(ctypes.Structure):
        _fields_ = [("width", ctypes.c_double), ("height", ctypes.c_double)]

    class Rect(ctypes.Structure):
        _fields_ = [("origin", Point), ("size", Size)]

    def message(obj, selector, result=ctypes.c_void_p, types=(), values=()):
        call = ctypes.CFUNCTYPE(result, ctypes.c_void_p, ctypes.c_void_p, *types)(
            ("objc_msgSend", objc)
        )
        return call(obj, objc.sel_registerName(selector.encode()), *values)

    screens = message(objc.objc_getClass(b"NSScreen"), "screens")
    primary = message(screens, "objectAtIndex:", types=(ctypes.c_ulong,), values=(0,))
    primary_frame = message(primary, "frame", Rect)
    top = primary_frame.origin.y + primary_frame.size.height
    windows = []
    for index in range(2):
        window = QMainWindow()
        window.setWindowTitle(f"{label}-{index}")
        window.setAttribute(Qt.WidgetAttribute.WA_TranslucentBackground)
        window.setAttribute(Qt.WidgetAttribute.WA_ShowWithoutActivating)
        window.resize(640, 480)
        window.show()
        native = message(int(window.winId()), "window")
        message(native, "setOpaque:", None, (ctypes.c_bool,), (False,))
        message(native, "setHasShadow:", None, (ctypes.c_bool,), (False,))
        clear = message(objc.objc_getClass(b"NSColor"), "clearColor")
        message(native, "setBackgroundColor:", None, (ctypes.c_void_p,), (clear,))
        message(native, "setTitlebarAppearsTransparent:", None, (ctypes.c_bool,), (True,))
        windows.append((window, native))

    def frames():
        result = {}
        for window, native in windows:
            frame = message(native, "frame", Rect)
            result[window.windowTitle()] = [
                frame.origin.x, top - frame.origin.y - frame.size.height,
                frame.size.width, frame.size.height,
            ]
        return result

    trace = []
    recording = False

    def poll():
        nonlocal recording
        if recording:
            trace.append({"time_ns": time.monotonic_ns(), "frames": frames()})
        if not select.select([sys.stdin], [], [], 0)[0]:
            return
        line = sys.stdin.readline()
        if not line:
            app.quit()
            return
        op = json.loads(line)["op"]
        if op == "quit":
            app.quit()
            return
        if op == "start":
            trace.clear()
            recording = True
        if op == "trace":
            recording = False
            print(json.dumps(trace), flush=True)
        else:
            print(json.dumps({"time_ns": time.monotonic_ns(), "frames": frames()}), flush=True)

    timer = QTimer()
    timer.timeout.connect(poll)
    timer.start(8)
    print(json.dumps({"pid": os.getpid()}), flush=True)
    app.exec()


def run(args):
    spec = importlib.util.spec_from_file_location(
        "rift_focus_check", Path(__file__).with_name("rift-settings-focus-live-test.py")
    )
    focus_check = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(focus_check)
    focus_check.require_unlocked()
    client = str(args.client.resolve())

    def execute(*command):
        focus_check.require_unlocked()
        subprocess.run([client, "execute", *command], check=True,
                       stdout=subprocess.DEVNULL, timeout=4)

    def query(kind):
        return json.loads(subprocess.check_output([client, "query", kind], timeout=4))

    def focus(window):
        execute("window", "focus", "--window-id", json.dumps(window["id"]),
                "--window-server-id", str(window["window_server_id"]))

    original = next((w for w in query("windows") if w["is_focused"]), None)
    workspaces = query("workspaces")
    active = next(w for w in workspaces if w["is_focused"])
    empty = next((w for w in workspaces if w["display_uuid"] == active["display_uuid"]
                  and w["window_count"] == 0 and w["layout_mode"] == "scrolling"), None)
    if empty is None:
        raise RuntimeError("An empty scrolling workspace on the focused display is required")
    report = {"label": "rift-motion-" + uuid.uuid4().hex[:8], "scenes": [], "failures": []}
    label = report["label"]
    child = None
    execute("workspace", "switch", str(empty["index"]))
    try:
        child = subprocess.Popen(
            [str(args.qt_python), "-u", str(Path(__file__).resolve()), "--fixture", label],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True,
            env=dict(os.environ, PYTHONPATH=str(args.qt_packages.resolve())),
        )

        def reply():
            if not select.select([child.stdout], [], [], 5)[0]:
                raise AssertionError("Native fixture did not respond")
            line = child.stdout.readline()
            if not line:
                raise AssertionError(f"Native fixture exited with {child.poll()}")
            return json.loads(line)

        def command(op):
            child.stdin.write(json.dumps({"op": op}) + "\n")
            child.stdin.flush()
            return reply()

        pid = reply()["pid"]

        def windows():
            return [w for w in query("windows") if w["id"]["pid"] == pid]

        deadline = time.monotonic() + 8
        while len(windows()) != 2:
            assert time.monotonic() < deadline, "Rift did not manage both fixture windows"
            time.sleep(0.1)
        target = next(w for w in windows() if w["title"] == label + "-0")
        focus(target)
        time.sleep(1)

        def scene(name, operations, axis, returns_to_start=False):
            initial = command("start")["frames"][target["title"]][axis]
            events = []
            for delay, operation in operations:
                time.sleep(delay)
                events.append({"time_ns": time.monotonic_ns(), "command": operation})
                execute(*operation)
            time.sleep(1)
            samples = command("trace")
            current = windows()
            record = {"name": name, "events": events, "samples": samples, "windows": current}
            report["scenes"].append(record)
            values = [s["frames"][target["title"]][axis] for s in samples]
            low, high = min(values), max(values)
            interior = {round(v, 1) for v in values if low + 1 < v < high - 1}
            try:
                assert high - low > 10, f"{name}: fixture geometry did not change"
                assert len(interior) >= 3, f"{name}: resize jumped without intermediate native sizes"
                if returns_to_start:
                    assert abs(values[-1] - initial) <= 1, (
                        f"{name}: reversal ended at {values[-1]}, expected {initial}"
                    )
                assert len(current) == 2, f"{name}: fixture window disappeared"
                settled = samples[-1]["frames"]
                for window in current:
                    frame = window["frame"]
                    expected = [frame["origin"]["x"], frame["origin"]["y"],
                                frame["size"]["width"], frame["size"]["height"]]
                    actual = settled[window["title"]]
                    assert all(abs(a - b) <= 1 for a, b in zip(actual, expected)), (
                        f"{name}: native frame {actual} differs from target {expected}"
                    )
                    tail = [s["frames"][window["title"]] for s in samples
                            if s["time_ns"] >= samples[-1]["time_ns"] - 150_000_000]
                    assert all(all(abs(a - b) <= 1 for a, b in zip(item, actual)) for item in tail), (
                        f"{name}: native geometry did not settle after the last target"
                    )
                print(f"PASS {name}: intermediate native sizes and settled target", flush=True)
            except AssertionError as error:
                report["failures"].append(str(error))
                print(f"FAIL {error}", flush=True)

        preset = ["layout", "switch-preset-column-width"]
        scene("column-resize", [(0, preset)], 2)
        scene("column-retarget", [(0, preset), (0.045, preset)], 2, returns_to_start=True)
        before_join = windows()
        selected_column = next(w for w in before_join if w["id"] == target["id"])["layout_position"]["column"]
        other_column = next(w for w in before_join if w["id"] != target["id"])["layout_position"]["column"]
        execute("layout", "join-window", "right" if other_column > selected_column else "left")
        time.sleep(1)
        joined = windows()
        assert len({w["layout_position"]["column"] for w in joined}) == 1, "Fixture rows did not join"
        grow = ["window", "resize-grow", "--orientation", "vertical"]
        shrink = ["window", "resize-shrink", "--orientation", "vertical"]
        scene("row-resize", [(0, grow)], 3)
        scene("row-retarget", [(0, shrink), (0.045, grow)], 3, returns_to_start=True)
    finally:
        if args.output:
            args.output.parent.mkdir(parents=True, exist_ok=True)
            args.output.write_text(json.dumps(report, indent=2) + "\n")
        try:
            if child is not None and child.poll() is None:
                try:
                    child.stdin.write('{"op":"quit"}\n')
                    child.stdin.flush()
                    child.wait(timeout=3)
                except (BrokenPipeError, subprocess.TimeoutExpired):
                    child.terminate()
                    child.wait(timeout=3)
        finally:
            execute("workspace", "switch", str(active["index"]))
            if original:
                focus(original)
    if report["failures"]:
        raise AssertionError("; ".join(report["failures"]))


if __name__ == "__main__":
    if len(sys.argv) == 3 and sys.argv[1] == "--fixture":
        fixture(sys.argv[2])
    else:
        parser = argparse.ArgumentParser(description="Verify live Rift resize and retarget motion using sampled native window frames.")
        parser.add_argument("--client", type=Path, default=Path(__file__).resolve().parents[1] / "target/debug/rift-cli")
        parser.add_argument("--qt-python", type=Path, default=Path("/Applications/Anki.app/Contents/Frameworks/Python.framework/Versions/3.13/bin/python3.13"))
        parser.add_argument("--qt-packages", type=Path, default=Path("/Applications/Anki.app/Contents/Resources/app_packages"))
        parser.add_argument("--output", type=Path, help="Save native samples and command timestamps, including failures.")
        args = parser.parse_args()
        if not args.qt_python.is_file() or not args.qt_packages.is_dir():
            parser.error("Provide --qt-python and --qt-packages for an installed PyQt6 runtime")
        run(args)
