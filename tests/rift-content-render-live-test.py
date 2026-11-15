#!/usr/bin/env python3
# Modified by kdnk for wm on 2026-11-15. Original attribution is preserved in LICENSE.
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


def validate_content(initial, records, title, axis):
    try:
        samples = [record for record in records if record["kind"] == "sample"]
        assert samples, f"{title}: no content samples recorded"
        content = [snapshot["content"][title] for snapshot in [initial, *samples]]
        for state in content:
            assert isinstance(state["generation"], int) and state["generation"] >= 0, (
                f"{title}: invalid paint generation"
            )
            assert len(state["size"]) == 2 and all(value > 0 for value in state["size"]), (
                f"{title}: invalid content size"
            )
        settled = content[-1]
        native_frames = [snapshot["frames"][title] for snapshot in [initial, *samples]]
        insets = [native_frames[0][dimension + 2] - content[0]["size"][dimension]
                  for dimension in range(2)]

        def matches_native(frame, size):
            return all(abs(frame[dimension + 2] - size[dimension] - insets[dimension]) <= 1
                       for dimension in range(2))

        native = [frame[axis] for frame in native_frames]
        dimension = axis - 2
        assert matches_native(native_frames[-1], settled["size"]), (
            f"{title}: settled content size does not follow the native resize"
        )
        assert settled["painted_size"] == settled["size"], (
            f"{title}: settled content has stale painted size "
            f"{settled['painted_size']}, expected {settled['size']}"
        )
        paints = [record for record in records
                  if record["kind"] == "paint_end" and record["title"] == title]
        sizes = [state["size"] for state in content] + [paint["content_size"] for paint in paints]
        changed = any(size != content[0]["size"] for size in sizes) or any(
            abs(frame[dimension + 2] - native_frames[0][dimension + 2]) > 1
            for frame in native_frames for dimension in range(2)
        )
        stale_samples = sum(state["painted_size"] != state["size"] for state in content[1:])
        checks = {"paint_count": len(paints), "stale_samples": stale_samples,
                  "native_size_mismatches": sum(not matches_native(frame, state["size"])
                                                for frame, state in zip(native_frames[1:], content[1:]))}
        if not changed:
            return checks
        assert settled["generation"] > content[0]["generation"], (
            f"{title}: paint generation did not advance during resizing"
        )
        fresh = [paint for paint in paints if paint["generation"] > content[0]["generation"]]
        assert fresh, f"{title}: resized content has no completed paint"
        assert (fresh[-1]["generation"], fresh[-1]["content_size"]) == (
            settled["generation"], settled["size"]
        ), f"{title}: final paint does not match the settled content generation and size"
        low, high = min(native), max(native)
        interior = {round(value, 1) for value in native if low + 1 < value < high - 1}
        if len(interior) >= 3:
            content_low = min(size[dimension] for size in sizes)
            content_high = max(size[dimension] for size in sizes)
            assert any(
                low + 1 < paint["native"][axis] < high - 1
                and content_low < paint["content_size"][dimension] < content_high
                and matches_native(paint["native"], paint["content_size"])
                for paint in fresh
            ), f"{title}: content painted only at endpoints of a native resize"
        return checks
    except (KeyError, TypeError, IndexError) as error:
        raise AssertionError(f"{title}: malformed content trace: {error}") from error


def fixture(label):
    from PyQt6.QtCore import QTimer, Qt
    from PyQt6.QtGui import QColor, QPainter, QFont
    from PyQt6.QtWidgets import QApplication, QMainWindow, QWidget

    app = QApplication([])
    app.setApplicationName("Wm content fixture")
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
        call = ctypes.CFUNCTYPE(result, ctypes.c_void_p, ctypes.c_void_p, *types)(("objc_msgSend", objc))
        return call(obj, objc.sel_registerName(selector.encode()), *values)

    screens = message(objc.objc_getClass(b"NSScreen"), "screens")
    primary = message(screens, "objectAtIndex:", types=(ctypes.c_ulong,), values=(0,))
    primary_frame = message(primary, "frame", Rect)
    top = primary_frame.origin.y + primary_frame.size.height
    trace = []
    recording = False

    def native_frame(window):
        native = getattr(window, "native", None)
        if native is None:
            return None
        frame = message(native, "frame", Rect)
        return [frame.origin.x, top - frame.origin.y - frame.size.height, frame.size.width, frame.size.height]

    def event(kind, window, **details):
        if recording:
            trace.append({"kind": kind, "time_ns": time.clock_gettime_ns(time.CLOCK_MONOTONIC_RAW), "title": window.windowTitle(),
                          "native": native_frame(window), "qt_size": [window.width(), window.height()], **details})

    class Content(QWidget):
        def __init__(self, window):
            super().__init__(window)
            self.window = window
            self.generation = 0
            self.painted_size = None
            self.setAttribute(Qt.WidgetAttribute.WA_OpaquePaintEvent)
        def paintEvent(self, paint_event):
            self.generation += 1
            started = time.clock_gettime_ns(time.CLOCK_MONOTONIC_RAW)
            event("paint_start", self.window, generation=self.generation, content_size=[self.width(), self.height()])
            painter = QPainter(self)
            painter.fillRect(self.rect(), QColor("#17202a"))
            for y in range(40, self.height(), 40):
                painter.fillRect(0, y, self.width(), 1, QColor("#34495e"))
            for x in range(40, self.width(), 40):
                painter.fillRect(x, 0, 1, self.height(), QColor("#34495e"))
            painter.fillRect(self.width() - 8, 0, 8, self.height(), QColor("#ff3030"))
            painter.fillRect(0, self.height() - 8, self.width(), 8, QColor("#30ff60"))
            painter.setPen(QColor("#ffffff"))
            painter.setFont(QFont("Menlo", 18))
            painter.drawText(20, 30, f"paint {self.generation}  content {self.width()} x {self.height()}")
            painter.setFont(QFont("Menlo", 13))
            text = "Resize content probe. Text layout follows the actual content width. " * 100
            painter.drawText(self.rect().adjusted(20, 60, -30, -30), Qt.TextFlag.TextWordWrap, text)
            painter.end()
            self.painted_size = [self.width(), self.height()]
            event("paint_end", self.window, generation=self.generation, content_size=self.painted_size,
                  paint_started_ns=started)

    class Window(QMainWindow):
        def resizeEvent(self, resize_event):
            super().resizeEvent(resize_event)
            event("resize", self, old_size=[resize_event.oldSize().width(), resize_event.oldSize().height()])
        def moveEvent(self, move_event):
            super().moveEvent(move_event)
            event("move", self, qt_position=[self.x(), self.y()])

    window_count = int(os.environ.get("WM_FIXTURE_WINDOW_COUNT", "2"))
    if not 1 <= window_count <= 8:
        raise ValueError("WM_FIXTURE_WINDOW_COUNT must be between 1 and 8")
    windows = []
    for index in range(window_count):
        window = Window()
        window.setWindowTitle(f"{label}-{index}")
        window.setAttribute(Qt.WidgetAttribute.WA_TranslucentBackground)
        window.setAttribute(Qt.WidgetAttribute.WA_ShowWithoutActivating)
        content = Content(window)
        window.setCentralWidget(content)
        window.resize(640, 480)
        window.show()
        window.native = message(int(window.winId()), "window")
        message(window.native, "setOpaque:", None, (ctypes.c_bool,), (False,))
        message(window.native, "setHasShadow:", None, (ctypes.c_bool,), (False,))
        clear = message(objc.objc_getClass(b"NSColor"), "clearColor")
        message(window.native, "setBackgroundColor:", None, (ctypes.c_void_p,), (clear,))
        message(window.native, "setTitlebarAppearsTransparent:", None, (ctypes.c_bool,), (True,))
        windows.append((window, content))

    def frames():
        return {w.windowTitle(): native_frame(w) for w, _ in windows}

    def focused_window():
        return next((w.windowTitle() for w, _ in windows
                     if message(w.native, "isKeyWindow", ctypes.c_bool)), None)

    def content_states():
        return {w.windowTitle(): {"generation": c.generation, "painted_size": c.painted_size,
                                  "size": [c.width(), c.height()]} for w, c in windows}

    def poll():
        nonlocal recording
        if recording:
            trace.append({"kind": "sample", "time_ns": time.clock_gettime_ns(time.CLOCK_MONOTONIC_RAW), "frames": frames(),
                          "content": content_states(), "focused_window": focused_window()})
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
            print(json.dumps({"time_ns": time.clock_gettime_ns(time.CLOCK_MONOTONIC_RAW), "frames": frames(),
                              "content": content_states(), "focused_window": focused_window()}), flush=True)

    timer = QTimer()
    timer.timeout.connect(poll)
    timer.start(8)
    print(json.dumps({"pid": os.getpid()}), flush=True)
    app.exec()

def run(args):
    spec = importlib.util.spec_from_file_location(
        "wm_focus_check", Path(__file__).with_name("rift-settings-focus-live-test.py")
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
    report = {"label": "wm-motion-" + uuid.uuid4().hex[:8], "scenes": [], "failures": []}
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
            assert time.monotonic() < deadline, "Wm did not manage both fixture windows"
            time.sleep(0.1)
        target = next(w for w in windows() if w["title"] == label + "-0")
        focus(target)
        time.sleep(1)

        def scene(name, operations, axis, returns_to_start=False):
            recorder = None
            if args.video_dir:
                args.video_dir.mkdir(parents=True, exist_ok=True)
                recorder = subprocess.Popen([
                    '/usr/sbin/screencapture', '-x', '-v', '-V', '2', '-l',
                    str(target['window_server_id']), str(args.video_dir / (name + '.mov')),
                ], stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
                time.sleep(0.25)
            initial = command("start")
            events = []
            for delay, operation in operations:
                time.sleep(delay)
                events.append({"time_ns": time.clock_gettime_ns(time.CLOCK_MONOTONIC_RAW), "command": operation})
                execute(*operation)
            time.sleep(1)
            records = command("trace")
            samples = [r for r in records if r["kind"] == "sample"]
            current = windows()
            video_result = None
            if recorder:
                _, error = recorder.communicate(timeout=6)
                video_result = {'returncode': recorder.returncode, 'stderr': error.decode()}
            record = {"name": name, "video": video_result, "events": events, "initial": initial,
                      "samples": samples, "events_from_app": [r for r in records if r["kind"] != "sample"],
                      "windows": current}
            report["scenes"].append(record)
            values = [s["frames"][target["title"]][axis] for s in samples]
            low, high = min(values), max(values)
            interior = {round(v, 1) for v in values if low + 1 < v < high - 1}
            try:
                if video_result:
                    assert video_result["returncode"] == 0, (
                        f"{name}: fixture video failed: {video_result['stderr']}"
                    )
                assert high - low > 10, f"{name}: fixture geometry did not change"
                assert len(interior) >= 3, f"{name}: resize jumped without intermediate native sizes"
                if returns_to_start:
                    initial_size = initial["frames"][target["title"]][axis]
                    assert abs(values[-1] - initial_size) <= 1, (
                        f"{name}: reversal ended at {values[-1]}, expected {initial_size}"
                    )
                assert len(current) == 2, f"{name}: fixture window disappeared"
                settled = samples[-1]["frames"]
                record["content_checks"] = {}
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
                    record["content_checks"][window["title"]] = validate_content(
                        initial, records, window["title"], axis
                    )
                print("PASS geometry and content", name, record["content_checks"], flush=True)
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
        parser = argparse.ArgumentParser(description="Record Wm native geometry and real content paint events during resizing.")
        parser.add_argument("--client", type=Path, default=Path(__file__).resolve().parents[1] / "target/debug/wm")
        parser.add_argument("--qt-python", type=Path, default=Path("/Applications/Anki.app/Contents/Frameworks/Python.framework/Versions/3.13/bin/python3.13"))
        parser.add_argument("--qt-packages", type=Path, default=Path("/Applications/Anki.app/Contents/Resources/app_packages"))
        parser.add_argument("--output", type=Path, help="Save native samples and command timestamps, including failures.")
        parser.add_argument("--video-dir", type=Path, help="Capture fixture window only during each scene.")
        args = parser.parse_args()
        if not args.qt_python.is_file() or not args.qt_packages.is_dir():
            parser.error("Provide --qt-python and --qt-packages for an installed PyQt6 runtime")
        run(args)
