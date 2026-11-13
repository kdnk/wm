#!/usr/bin/env python3
import argparse
import json
import os
from pathlib import Path
import select
import subprocess
import sys
import time
import uuid


def fixture(label):
    import ctypes
    from PyQt6.QtCore import QTimer, Qt
    from PyQt6.QtWidgets import QApplication, QMainWindow

    app = QApplication([])
    app.setApplicationName("Rift lifecycle fixture")
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

    def native(window):
        return message(int(window.winId()), "window")

    windows = []
    for index in range(3):
        window = QMainWindow()
        window.setWindowTitle(f"{label}-{index}")
        window.setAttribute(Qt.WidgetAttribute.WA_TranslucentBackground)
        window.setAttribute(Qt.WidgetAttribute.WA_ShowWithoutActivating)
        window.resize(640, 480)
        window.show()
        handle = native(window)
        message(handle, "setOpaque:", None, (ctypes.c_bool,), (False,))
        message(handle, "setHasShadow:", None, (ctypes.c_bool,), (False,))
        clear = message(objc.objc_getClass(b"NSColor"), "clearColor")
        message(handle, "setBackgroundColor:", None, (ctypes.c_void_p,), (clear,))
        message(handle, "setTitlebarAppearsTransparent:", None, (ctypes.c_bool,), (True,))
        windows.append(window)

    def poll():
        if not select.select([sys.stdin], [], [], 0)[0]:
            return
        line = sys.stdin.readline()
        if not line:
            app.quit()
            return
        command = json.loads(line)
        op = command["op"]
        if op == "quit":
            app.quit()
            return
        window = windows[1]
        if op == "close":
            window.close()
        elif op == "show":
            window.show()
        elif op == "native-hide":
            message(native(window), "orderOut:", None, (ctypes.c_void_p,), (None,))
        elif op == "native-show":
            message(native(window), "orderFront:", None, (ctypes.c_void_p,), (None,))
        elif op == "minimize":
            window.showMinimized()
        elif op == "restore":
            window.showNormal()
        elif op != "state":
            raise ValueError(op)
        state = {"op": op, "qt_visible": window.isVisible()}
        if window.isVisible():
            handle = native(window)
            state["key_window"] = message(handle, "isKeyWindow", ctypes.c_bool)
            state["window_server_id"] = message(handle, "windowNumber", ctypes.c_long)
        frames = []
        for candidate in windows:
            if not candidate.isVisible() or candidate.isMinimized():
                continue
            handle = native(candidate)
            if not message(handle, "isVisible", ctypes.c_bool):
                continue
            frame = message(handle, "frame", Rect)
            frames.append({"title": candidate.windowTitle(), "x": frame.origin.x, "width": frame.size.width})
        state["native_frames"] = frames
        print(json.dumps(state), flush=True)

    timer = QTimer()
    timer.timeout.connect(poll)
    timer.start(20)
    print(json.dumps({"pid": os.getpid()}), flush=True)
    app.exec()


def run(args):
    client = str(args.client.resolve())

    def query(kind):
        return json.loads(subprocess.check_output([client, "query", kind], timeout=3))

    def focus(window):
        subprocess.run(
            [client, "execute", "window", "focus", "--window-id", json.dumps(window["id"]),
             "--window-server-id", str(window["window_server_id"])],
            check=True, stdout=subprocess.DEVNULL, timeout=3,
        )

    def switch(index):
        subprocess.run([client, "execute", "workspace", "switch", str(index)],
                       check=True, stdout=subprocess.DEVNULL, timeout=3)

    original = next((w for w in query("windows") if w["is_focused"]), None)
    workspaces = query("workspaces")
    active = next(w for w in workspaces if w["is_focused"])
    empty = next((w for w in workspaces if w["display_uuid"] == active["display_uuid"]
                  and w["window_count"] == 0 and w["layout_mode"] == "scrolling"), None)
    if empty is None:
        raise RuntimeError("An empty scrolling workspace on the focused display is required")
    switch(empty["index"])
    label = "rift-lifecycle-" + uuid.uuid4().hex[:8]
    env = dict(os.environ, PYTHONPATH=str(args.qt_packages.resolve()))
    try:
        child = subprocess.Popen(
            [str(args.qt_python), "-u", str(Path(__file__).resolve()), "--fixture", label],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True, env=env,
        )
    except OSError:
        switch(active["index"])
        raise

    def reply():
        if not select.select([child.stdout], [], [], 5)[0]:
            raise AssertionError("Qt fixture did not respond")
        line = child.stdout.readline()
        if not line:
            raise AssertionError(f"Qt fixture exited with {child.poll()}")
        return json.loads(line)

    def command(op):
        child.stdin.write(json.dumps({"op": op}) + "\n")
        child.stdin.flush()
        return reply()

    try:
        pid = reply()["pid"]

        def snapshot():
            windows = [w for w in query("windows") if w["id"]["pid"] == pid]
            return windows

        def await_count(count):
            deadline = time.monotonic() + 8
            while time.monotonic() < deadline:
                windows = snapshot()
                if len(windows) == count:
                    return windows
                time.sleep(0.1)
            raise AssertionError(f"Expected {count} fixture windows: {windows}")

        def await_packed(count):
            deadline = time.monotonic() + 5
            while time.monotonic() < deadline:
                frames = sorted(command("state")["native_frames"], key=lambda f: f["x"])
                if len(frames) == count and all(
                    abs(right["x"] - left["x"] - left["width"] - args.gap) <= 1
                    for left, right in zip(frames, frames[1:])
                ):
                    return
                time.sleep(0.1)
            raise AssertionError(f"Native windows must have no vacant tile between them: {frames}")

        def target(windows):
            return next(w for w in windows if w["title"] == f"{label}-1")

        def order(windows):
            return [w["title"] for w in sorted(windows, key=lambda w: (
                w["layout_position"]["column"], w["layout_position"]["row"]
            ))]

        def await_order(expected):
            deadline = time.monotonic() + 5
            while time.monotonic() < deadline:
                windows = snapshot()
                if len(windows) == 3 and order(windows) == expected:
                    return windows
                time.sleep(0.1)
            raise AssertionError(f"Restoration changed column order: expected {expected}, actual {order(windows)}")

        def settled_windows():
            deadline = time.monotonic() + 5
            previous = None
            stable_since = time.monotonic()
            while time.monotonic() < deadline:
                windows = snapshot()
                frames = sorted(command("state")["native_frames"], key=lambda f: f["x"])
                observed = order(windows)
                if len(windows) == 3 and observed == [f["title"] for f in frames]:
                    if observed == previous and time.monotonic() - stable_since >= 0.5:
                        return windows
                    if observed != previous:
                        previous = observed
                        stable_since = time.monotonic()
                else:
                    previous = None
                    stable_since = time.monotonic()
                time.sleep(0.1)
            raise AssertionError(f"Native geometry and queried column order did not settle: {windows}, {frames}")

        windows = await_count(3)
        await_packed(3)
        initial_order = order(settled_windows())
        for cycle in range(args.cycles):
            live = target(windows)
            command("native-hide")
            time.sleep(0.5)
            state = command("native-show")
            assert state["window_server_id"] == live["window_server_id"], "Native hide replaced the window"
            windows = await_count(3)
            await_packed(3)
            windows = await_order(initial_order)
            live = target(windows)
            focus(live)
            deadline = time.monotonic() + 5
            while not command("state")["key_window"]:
                assert time.monotonic() < deadline, "Restored native window could not receive focus"
                time.sleep(0.1)
            print(f"PASS cycle {cycle + 1}: native ordering out restores the same window, column order, and native focus", flush=True)

            command("minimize")
            await_count(2)
            await_packed(2)
            command("restore")
            windows = await_count(3)
            await_packed(3)
            windows = await_order(initial_order)
            print(f"PASS cycle {cycle + 1}: minimizing removes the tile and restoration preserves column order", flush=True)

            command("close")
            windows = await_count(2)
            await_packed(2)
            command("show")
            windows = await_count(3)
            await_packed(3)
            assert len({w["window_server_id"] for w in windows}) == 3, "Reopening duplicated an identity"
            initial_order = order(settled_windows())
            print(f"PASS cycle {cycle + 1}: closing removes the tile and retained Qt object reopens", flush=True)
    finally:
        try:
            if child.poll() is None:
                try:
                    child.stdin.write('{"op":"quit"}\n')
                    child.stdin.flush()
                except BrokenPipeError:
                    pass
                try:
                    child.wait(timeout=3)
                except subprocess.TimeoutExpired:
                    child.terminate()
                    child.wait(timeout=3)
        finally:
            switch(active["index"])
            if original:
                focus(original)


if __name__ == "__main__":
    if len(sys.argv) == 3 and sys.argv[1] == "--fixture":
        fixture(sys.argv[2])
    else:
        parser = argparse.ArgumentParser(description="Verify live Rift close, native hide, and minimize lifecycles with transparent Qt windows.")
        parser.add_argument("--client", type=Path, default=Path(__file__).resolve().parents[1] / "target/debug/rift-cli")
        parser.add_argument("--qt-python", type=Path, default=Path("/Applications/Anki.app/Contents/Frameworks/Python.framework/Versions/3.13/bin/python3.13"))
        parser.add_argument("--qt-packages", type=Path, default=Path("/Applications/Anki.app/Contents/Resources/app_packages"))
        parser.add_argument("--cycles", type=int, default=2)
        parser.add_argument("--gap", type=float, default=0)
        args = parser.parse_args()
        if args.cycles < 1:
            parser.error("--cycles must be positive")
        if not args.qt_python.is_file() or not args.qt_packages.is_dir():
            parser.error("Provide --qt-python and --qt-packages for an installed PyQt6 runtime")
        run(args)
