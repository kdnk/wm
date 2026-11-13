#!/usr/bin/env python3
# Modified by kdnk for wm on 2026-11-14. Original attribution is preserved in LICENSE.
"""Exercise the installed Wm presenter and retain content evidence for review."""

import argparse
import ctypes as C
import importlib.util
import json
import math
import os
from pathlib import Path
import select
import subprocess
import sys
import threading
import time
import uuid

sys.dont_write_bytecode = True

ROOT = Path(__file__).resolve().parents[1]
PRESET = ["layout", "switch-preset-column-width"]


def module(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    loaded = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(loaded)
    return loaded


def rect(frame):
    return [frame["origin"]["x"], frame["origin"]["y"],
            frame["size"]["width"], frame["size"]["height"]]


def near(left, right, tolerance=2):
    return len(left) == len(right) and all(abs(a - b) <= tolerance for a, b in zip(left, right))


class Pair(C.Structure):
    _fields_ = [("a", C.c_double), ("b", C.c_double)]


class NativeWindowAbsent(LookupError):
    pass


class NativeWindow:
    def __init__(self, pid, window_id):
        self.ax = C.CDLL("/System/Library/Frameworks/ApplicationServices.framework/ApplicationServices")
        self.cf = C.CDLL("/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation")
        for name, result, arguments in [
            ("AXUIElementCreateApplication", C.c_void_p, [C.c_int]),
            ("AXUIElementCopyAttributeValue", C.c_int, [C.c_void_p, C.c_void_p, C.POINTER(C.c_void_p)]),
            ("AXValueGetValue", C.c_bool, [C.c_void_p, C.c_int, C.c_void_p]),
            ("_AXUIElementGetWindow", C.c_int, [C.c_void_p, C.POINTER(C.c_uint32)]),
            ("AXUIElementPerformAction", C.c_int, [C.c_void_p, C.c_void_p]),
        ]:
            function = getattr(self.ax, name)
            function.restype, function.argtypes = result, arguments
        for name, result, arguments in [
            ("CFStringCreateWithCString", C.c_void_p, [C.c_void_p, C.c_char_p, C.c_uint32]),
            ("CFArrayGetCount", C.c_long, [C.c_void_p]),
            ("CFArrayGetValueAtIndex", C.c_void_p, [C.c_void_p, C.c_long]),
            ("CFRetain", C.c_void_p, [C.c_void_p]),
            ("CFRelease", None, [C.c_void_p]),
        ]:
            function = getattr(self.cf, name)
            function.restype, function.argtypes = result, arguments
        self.application = self.ax.AXUIElementCreateApplication(pid)
        self.window = None
        try:
            windows = self.get(self.application, "AXWindows")
        except Exception:
            self.dispose()
            raise
        try:
            for index in range(self.cf.CFArrayGetCount(windows)):
                candidate = self.cf.CFArrayGetValueAtIndex(windows, index)
                actual = C.c_uint32()
                status = self.ax._AXUIElementGetWindow(candidate, C.byref(actual))
                if status == 0 and actual.value == window_id:
                    self.window = self.cf.CFRetain(candidate)
                    break
        finally:
            self.cf.CFRelease(windows)
        if not self.window:
            self.dispose()
            raise NativeWindowAbsent(f"Native window {window_id} is absent from application {pid}")

    def get(self, owner, name):
        key = self.cf.CFStringCreateWithCString(None, name.encode(), 0x08000100)
        value = C.c_void_p()
        try:
            status = self.ax.AXUIElementCopyAttributeValue(owner, key, C.byref(value))
            if status != 0:
                raise RuntimeError(f"Native {name} read failed with AX error {status}")
            return value.value
        finally:
            self.cf.CFRelease(key)

    def frame(self):
        result = []
        for name, kind in [("AXPosition", 1), ("AXSize", 2)]:
            value = self.get(self.window, name)
            pair = Pair()
            try:
                if not self.ax.AXValueGetValue(value, kind, C.byref(pair)):
                    raise RuntimeError(f"Cannot decode native {name}")
                result.extend([pair.a, pair.b])
            finally:
                self.cf.CFRelease(value)
        return result

    def close(self):
        button = self.get(self.window, "AXCloseButton")
        action = self.cf.CFStringCreateWithCString(None, b"AXPress", 0x08000100)
        try:
            status = self.ax.AXUIElementPerformAction(button, action)
            if status != 0:
                raise RuntimeError(f"Closing the disposable window failed with AX error {status}")
        finally:
            self.cf.CFRelease(button)
            self.cf.CFRelease(action)

    def focused_window_id(self):
        focused = self.get(self.application, "AXFocusedWindow")
        identifier = C.c_uint32()
        try:
            status = self.ax._AXUIElementGetWindow(focused, C.byref(identifier))
            if status != 0:
                raise RuntimeError(f"Focused window identity read failed with AX error {status}")
            return identifier.value
        finally:
            self.cf.CFRelease(focused)

    def dispose(self):
        for name in ["window", "application"]:
            value = getattr(self, name, None)
            if value:
                self.cf.CFRelease(value)
                setattr(self, name, None)


def native_window_present(window, native_factory=NativeWindow):
    try:
        native = native_factory(window["id"]["pid"], window["window_server_id"])
    except NativeWindowAbsent:
        return False
    native.dispose()
    return True


def close_owned_window(window, native_factory=NativeWindow):
    try:
        native = native_factory(window["id"]["pid"], window["window_server_id"])
    except NativeWindowAbsent:
        return
    try:
        native.close()
    finally:
        native.dispose()
    deadline = time.monotonic() + 3
    while native_window_present(window, native_factory):
        if time.monotonic() >= deadline:
            raise RuntimeError(f"Disposable native window {window['window_server_id']} remained after close")
        time.sleep(0.05)


class Sampler:
    def __init__(self, window):
        self.window, self.records, self.errors = window, [], []
        self.stopped = threading.Event()
        self.thread = threading.Thread(target=self.run, daemon=True)

    def run(self):
        while not self.stopped.is_set():
            started = time.monotonic_ns()
            try:
                self.records.append({"time_ns": started, "frame": self.window.frame()})
            except RuntimeError as error:
                self.errors.append({"time_ns": started, "error": str(error)})
            self.stopped.wait(0.008)

    def __enter__(self):
        self.thread.start()
        return self

    def __exit__(self, *_):
        self.stopped.set()
        self.thread.join(timeout=5)
        if self.thread.is_alive():
            raise RuntimeError("Native frame sampler did not stop")


class Cursor:
    def __init__(self):
        self.cg = C.CDLL("/System/Library/Frameworks/CoreGraphics.framework/CoreGraphics")
        self.cf = C.CDLL("/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation")
        self.cg.CGEventCreate.argtypes, self.cg.CGEventCreate.restype = [C.c_void_p], C.c_void_p
        self.cg.CGEventGetLocation.argtypes, self.cg.CGEventGetLocation.restype = [C.c_void_p], Pair
        self.cg.CGWarpMouseCursorPosition.argtypes, self.cg.CGWarpMouseCursorPosition.restype = [Pair], C.c_int
        self.cf.CFRelease.argtypes = [C.c_void_p]

    def position(self):
        event = self.cg.CGEventCreate(None)
        if not event:
            raise RuntimeError("Cannot read the current mouse position")
        try:
            location = self.cg.CGEventGetLocation(event)
            return [location.a, location.b]
        finally:
            self.cf.CFRelease(event)

    def move(self, position):
        if self.cg.CGWarpMouseCursorPosition(Pair(*position)) != 0:
            raise RuntimeError("Cannot move the mouse into the disposable test window")


def frontmost_pid():
    class ProcessSerialNumber(C.Structure):
        _fields_ = [("high", C.c_uint32), ("low", C.c_uint32)]

    api = C.CDLL("/System/Library/Frameworks/ApplicationServices.framework/ApplicationServices")
    api.GetFrontProcess.argtypes = [C.POINTER(ProcessSerialNumber)]
    api.GetProcessPID.argtypes = [C.POINTER(ProcessSerialNumber), C.POINTER(C.c_int)]
    process, pid = ProcessSerialNumber(), C.c_int()
    status = api.GetFrontProcess(C.byref(process))
    if status == 0:
        status = api.GetProcessPID(C.byref(process), C.byref(pid))
    if status != 0:
        raise RuntimeError(f"Front process read failed with OSStatus {status}")
    return pid.value


def paint_counts(app_trace, title):
    events = app_trace.get("trace", []) if isinstance(app_trace, dict) else app_trace or []
    selected = [event for event in events if event.get("title", title) == title]
    return {kind: sum(event.get("kind") == kind for event in selected)
            for kind in ["resize", "paint_begin", "paint_start", "paint_end"]}


def trace_since(path, offset, window_id):
    if not path or not path.is_file():
        return [], offset
    with path.open("rb") as source:
        source.seek(offset)
        chunk = source.read()
    complete = chunk.rfind(b"\n") + 1
    events = []
    for line in chunk[:complete].splitlines():
        event = json.loads(line)
        if event.get("window_id") == window_id:
            events.append(event)
    return events, offset + complete


def summarize_sizes(samples):
    sizes = []
    for sample in samples:
        size = sample["frame"][2:]
        if not sizes or not near(size, sizes[-1], 0.5):
            sizes.append(size)
    return sizes


def layout_signature(node):
    return {
        "type": node["node_type"], "role": node.get("role"), "kind": node.get("layout_kind"),
        "window": node.get("window_id"), "weight": node.get("weight"), "size": node["frame"]["size"],
        "children": [layout_signature(child) for child in node["children"]],
    }


def run(args):
    guard = module("wm_focus_guard", ROOT / "tests/rift-settings-focus-live-test.py")
    guard.require_unlocked()
    client = str(args.client.resolve())

    def query(kind):
        return json.loads(subprocess.check_output([client, "query", kind], timeout=5))

    def execute(*command):
        guard.require_unlocked()
        subprocess.run([client, "execute", *command], check=True, timeout=5, stdout=subprocess.DEVNULL)

    def focus(window):
        execute("window", "focus", "--window-id", json.dumps(window["id"]),
                "--window-server-id", str(window["window_server_id"]))
        time.sleep(0.08)
        if frontmost_pid() != window["id"]["pid"]:
            workspace = next(workspace for workspace in query("workspaces") if workspace["is_focused"])
            alternative = next((candidate for candidate in workspace["windows"]
                                if candidate["id"]["pid"] != window["id"]["pid"]
                                and not candidate["is_floating"]), None)
            if alternative:
                report.setdefault("focus_reassertions", []).append({"from_pid": frontmost_pid(), "via": alternative["id"]})
                execute("window", "focus", "--window-id", json.dumps(alternative["id"]),
                        "--window-server-id", str(alternative["window_server_id"]))
                execute("window", "focus", "--window-id", json.dumps(window["id"]),
                        "--window-server-id", str(window["window_server_id"]))
                time.sleep(0.15)

    before = query("windows")
    original = next((window for window in before if window["is_focused"]), None)
    active = next(workspace for workspace in query("workspaces") if workspace["is_focused"])
    borrowed = next((window for window in before if window["window_server_id"] == args.window_id), None) if args.window_id else None
    if args.window_id and borrowed is None:
        raise RuntimeError(f"Requested borrowed window {args.window_id} is not managed in the current Wm window query")
    if borrowed and (borrowed["bundle_id"] != "com.google.Chrome" or borrowed["is_floating"] or borrowed["layout_position"] is None):
        raise RuntimeError("The borrowed window must be a normal managed Chrome window")
    if borrowed and not any(window["window_server_id"] == args.window_id for window in active["windows"]):
        raise RuntimeError("The borrowed window must belong to the currently focused workspace")
    empty = active if borrowed else next((workspace for workspace in query("workspaces")
                  if workspace["display_uuid"] == active["display_uuid"]
                  and workspace["window_count"] == 0 and workspace["layout_mode"] == "scrolling"), None)
    if not empty:
        raise RuntimeError("An empty scrolling workspace on the focused display is required")
    if empty["layout_mode"] != "scrolling":
        raise RuntimeError("The verification workspace must use scrolling layout")
    display = next(display for display in query("displays") if display["uuid"] == active["display_uuid"])
    capture_rect = ",".join(str(math.ceil(value)) for value in rect(display["frame"]))
    readiness = {"mode": args.mode, "owned_window": borrowed is None, "borrowed_window_id": args.window_id,
                 "workspace": empty, "display": display,
                 "trace_file": str(args.trace_file) if args.trace_file else None,
                 "would_run": ["50-to-100", "100-to-50", "preparation-retarget", "rapid-retarget"]
                 + (["right-half-to-full", "row-height"] if args.mode == "fixture" and args.fixture_bin is None else []),
                 "visual_quality": "Requires inspection of the display recordings."}
    if not args.run:
        print(json.dumps({"ready": True, "mutations": False, **readiness}, indent=2))
        return

    args.output.mkdir(parents=True, exist_ok=False)
    label = "wm-live-resize-" + uuid.uuid4().hex[:8]
    report = {**readiness, "label": label, "before_windows": before,
              "original_workspace": active, "scenes": [], "failures": []}
    if borrowed:
        report["before_layout"] = query("layout")
    fixture = recorder = native = created = cursor = original_cursor = cursor_target = None
    before_ids = {window["window_server_id"] for window in before}
    trace_offset = args.trace_file.stat().st_size if args.trace_file and args.trace_file.exists() else 0
    backup = args.output / "before-layout.ron"
    execute("save-layout", str(backup))

    def fixture_reply():
        if not select.select([fixture.stdout], [], [], 6)[0]:
            raise RuntimeError("Content fixture response timed out")
        line = fixture.stdout.readline()
        if not line:
            raise RuntimeError(f"Content fixture exited with status {fixture.poll()}")
        return json.loads(line)

    def fixture_command(op):
        fixture.stdin.write(json.dumps({"op": op}) + "\n")
        fixture.stdin.flush()
        return fixture_reply()

    try:
        if borrowed:
            created = borrowed
        else:
            execute("workspace", "switch", str(empty["index"]))
            if args.mode == "chrome":
                subprocess.Popen([str(args.chrome), "--new-window", args.url],
                                 stdout=(args.output / "chrome.stdout").open("w"),
                                 stderr=(args.output / "chrome.stderr").open("w"))
                report["chrome_launch_requested"] = True
                pid = None
            else:
                command = ([str(args.fixture_bin), "--hold"] if args.fixture_bin else
                           [str(args.qt_python), "-u", str(Path(__file__).resolve()), "--fixture", label])
                fixture = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                           stderr=(args.output / "fixture.stderr").open("w"), text=True,
                                           env=dict(os.environ, PYTHONPATH=str(args.qt_packages.resolve())))
                ready = fixture_reply()
                report["fixture_ready"] = ready
                pid = ready["pid"]
            deadline = time.monotonic() + 15
            while time.monotonic() < deadline:
                candidates = [window for window in query("windows")
                              if window["window_server_id"] not in before_ids
                              and (window["bundle_id"] == "com.google.Chrome" if pid is None
                                   else window["id"]["pid"] == pid)]
                if candidates:
                    if args.mode == "chrome" and len(candidates) != 1:
                        raise RuntimeError("Multiple Chrome windows appeared; cannot choose a disposable target safely")
                    created = next((window for window in candidates if window["title"] == label + "-0"), candidates[0])
                    break
                time.sleep(0.1)
            if not created:
                raise RuntimeError("Wm did not manage the disposable test window")
        if created["is_floating"] or created["layout_position"] is None:
            raise RuntimeError("The test window must use normal Wm tiling; a floating fixture cannot prove this path")
        focus(created)
        current = next(workspace for workspace in query("workspaces") if workspace["is_focused"])
        if current["index"] != empty["index"]:
            raise RuntimeError("Focus changed the verification workspace; no window was moved")
        time.sleep(2)
        native = NativeWindow(created["id"]["pid"], created["window_server_id"])
        report["target"] = created

        def target_context(stage):
            workspaces = query("workspaces")
            current = next((workspace for workspace in workspaces if workspace["is_focused"]), None)
            if current is None or current["index"] != empty["index"]:
                raise RuntimeError(f"{stage}: focus left verification workspace {empty['name']}")
            window = next((window for window in current["windows"]
                           if window["window_server_id"] == created["window_server_id"]), None)
            if window is None:
                raise RuntimeError(f"{stage}: target {created['window_server_id']} is absent from the active Wm workspace")
            if frontmost_pid() != created["id"]["pid"] or native.focused_window_id() != created["window_server_id"]:
                raise RuntimeError(f"{stage}: native focus left the verification target")
            return window
        initial = native.frame()
        cursor = Cursor()
        original_cursor = cursor.position()
        cursor_target = [initial[0] + min(500, initial[2] / 2), initial[1] + 12]
        cursor.move(cursor_target)
        focus(created)
        time.sleep(0.2)
        assert frontmost_pid() == created["id"]["pid"], "The disposable test application did not become frontmost"
        display_width = rect(display["frame"])[2]
        for _ in range(3):
            initial = native.frame()
            if abs(initial[2] - display_width * 0.5) <= 2:
                break
            target_context("preset setup")
            execute(*PRESET)
            time.sleep(args.settle)
        else:
            raise RuntimeError(f"Preset setup did not produce a half-width column: {initial}")
        cursor_target = [initial[0] + min(500, initial[2] / 2), initial[1] + 12]
        cursor.move(cursor_target)

        def scene(name, delays, return_to_start=False, operation=None, axis=2, require_motion=True):
            nonlocal recorder, trace_offset
            guard.require_unlocked()
            focus(created)
            cursor.move(cursor_target)
            time.sleep(0.15)
            target_context(name)
            start = native.frame()
            trace_offset = args.trace_file.stat().st_size if args.trace_file and args.trace_file.exists() else trace_offset
            fixture_start = fixture_command("start") if fixture else None
            video_started_us = None
            if args.video:
                video_started_us = time.time_ns() // 1000
                recorder = subprocess.Popen([
                    "/usr/sbin/screencapture", "-x", "-v", "-V", "8", "-R", capture_rect,
                    str(args.output / (name + ".mov")),
                ], stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
                time.sleep(0.45)
                focus(created)
                cursor.move(cursor_target)
                target_context(name + " recording setup")
            commands = []
            with Sampler(native) as sampler:
                time.sleep(0.03)
                for index, delay in enumerate(delays):
                    if return_to_start and require_motion and index:
                        deadline = time.monotonic() + 4
                        while time.monotonic() < deadline:
                            events, _ = trace_since(args.trace_file, trace_offset, created["window_server_id"])
                            if any(event.get("phase") == "animating" for event in events):
                                break
                            time.sleep(0.005)
                        else:
                            raise RuntimeError(f"{name}: the first transition never started compositor motion")
                    time.sleep(delay)
                    target_context(name)
                    command = operation or PRESET
                    commands.append({"time_ns": time.monotonic_ns(), "command": command})
                    execute(*command)
                time.sleep(args.settle)
            app_trace = fixture_command("trace") if fixture else None
            final = native.frame()
            current = target_context(name)
            proxy_trace, trace_offset = trace_since(args.trace_file, trace_offset, created["window_server_id"])
            video = None
            if recorder:
                _, stderr = recorder.communicate(timeout=10)
                video = {"path": str(args.output / (name + ".mov")),
                         "returncode": recorder.returncode, "stderr": stderr.decode(errors="replace"),
                         "started_us": video_started_us}
                recorder = None
            sizes = summarize_sizes(sampler.records)
            record = {"name": name, "from": start, "final": final, "commands": commands,
                      "native_samples": sampler.records, "native_read_errors": sampler.errors,
                      "native_sizes": sizes, "fixture_start": fixture_start, "app_trace": app_trace,
                      "proxy_trace": proxy_trace, "video": video, "managed_window": current,
                      "native_frontmost_pid_after": frontmost_pid(),
                      "native_focused_window_after": native.focused_window_id()}
            if fixture:
                record["content_event_counts"] = paint_counts(app_trace, created["title"])
            report["scenes"].append(record)
            (args.output / (name + ".json")).write_text(json.dumps(record, indent=2) + "\n")
            assert near(final, rect(current["frame"])), f"{name}: native frame differs from Wm target"
            assert not current["is_floating"], f"{name}: target unexpectedly became floating"
            assert record["native_frontmost_pid_after"] == created["id"]["pid"], f"{name}: native focus was not preserved"
            assert record["native_focused_window_after"] == created["window_server_id"], f"{name}: native window focus was not preserved"
            assert sampler.records, f"{name}: no native frame samples"
            assert len(sizes) <= len(delays) + 2, f"{name}: repeated native sizes show per-frame content reflow: {sizes}"
            if return_to_start:
                assert near(final, start), f"{name}: retarget did not return to the original frame"
            else:
                assert abs(final[axis] - start[axis]) > (display_width * 0.3 if axis == 2 else 2), f"{name}: requested size did not change"
            assert proxy_trace, f"{name}: no production proxy trace; ordinary geometry alone does not prove the content path"
            phases = {event.get("phase") for event in proxy_trace}
            if require_motion:
                assert "animating" in phases and "finished" in phases, f"{name}: compositor did not complete: {sorted(phases)}"
                assert max((event.get("live_updates", 0) for event in proxy_trace), default=0) > 0, f"{name}: production proxy did not report live target frames"
            if fixture and require_motion:
                counts = record["content_event_counts"]
                assert counts["paint_end"] > 0, f"{name}: the content fixture did not paint the changed layout"
                assert counts["resize"] <= len(delays) + 1, f"{name}: content repeatedly resized: {counts}"
            if video:
                assert video["returncode"] == 0, f"{name}: display recording failed: {video['stderr']}"
            print(json.dumps({"scene": name, "native_size_changes": len(sizes) - 1,
                              "proxy_events": len(proxy_trace), "final": final}), flush=True)

        scene("50-to-100", [0])
        scene("100-to-50", [0])
        scene("preparation-retarget", [0, 0.005], return_to_start=True, require_motion=False)
        scene("rapid-retarget", [0, 0.045], return_to_start=True)
        if fixture and args.fixture_bin is None:
            managed = [window for window in query("windows") if window["id"]["pid"] == created["id"]["pid"]]
            created = max(managed, key=lambda window: window["layout_position"]["column"])
            native.dispose()
            native = NativeWindow(created["id"]["pid"], created["window_server_id"])
            focus(created)
            time.sleep(args.settle)
            for _ in range(3):
                if abs(native.frame()[2] - display_width * 0.5) <= 2:
                    break
                target_context("right-column setup")
                execute(*PRESET)
                time.sleep(args.settle)
            right_frame = native.frame()
            assert abs(right_frame[2] - display_width * 0.5) <= 2, "Right column did not reach half width"
            assert right_frame[0] - rect(display["frame"])[0] > display_width * 0.4, "Right column was not on the right half"
            cursor_target = [right_frame[0] + min(500, right_frame[2] / 2), right_frame[1] + 12]
            scene("right-half-to-full", [0])
            origin_scene = report["scenes"][-1]
            assert abs(origin_scene["final"][0] - origin_scene["from"][0]) > display_width * 0.3, "Expansion did not exercise origin movement"
            managed = [window for window in query("windows") if window["id"]["pid"] == created["id"]["pid"]]
            sibling = next(window for window in managed if window["id"] != created["id"])
            direction = "right" if sibling["layout_position"]["column"] > created["layout_position"]["column"] else "left"
            focus(created)
            execute("layout", "join-window", direction)
            time.sleep(args.settle)
            managed = [window for window in query("windows") if window["id"]["pid"] == created["id"]["pid"]]
            assert len({window["layout_position"]["column"] for window in managed}) == 1, "Fixture rows did not join"
            created = min(managed, key=lambda window: window["layout_position"]["row"])
            native.dispose()
            native = NativeWindow(created["id"]["pid"], created["window_server_id"])
            row_frame = native.frame()
            cursor_target = [row_frame[0] + min(500, row_frame[2] / 2), row_frame[1] + 12]
            scene("row-height", [0], operation=["window", "resize-grow", "--orientation", "vertical"], axis=3)
    except Exception as error:
        report["failures"].append(f"{type(error).__name__}: {error}")
        report["failure_state"] = {}
        for kind in ["workspaces", "windows", "layout"]:
            try:
                report["failure_state"][kind] = query(kind)
            except Exception as query_error:
                report["failure_state"][kind] = {"error": str(query_error)}
        if native:
            try:
                report["failure_state"]["native_frame"] = native.frame()
                report["failure_state"]["native_focused_window_id"] = native.focused_window_id()
            except Exception as native_error:
                report["failure_state"]["native_error"] = str(native_error)
        raise
    finally:
        cleanup_errors = []
        restore_pointer = False
        if cursor is not None and cursor_target is not None and original_cursor is not None:
            try:
                restore_pointer = near(cursor.position(), cursor_target)
            except RuntimeError as error:
                cleanup_errors.append(str(error))
        if recorder:
            try:
                _, stderr = recorder.communicate(timeout=10)
                report["interrupted_video"] = {"path": recorder.args[-1], "returncode": recorder.returncode,
                                               "stderr": stderr.decode(errors="replace")}
            except subprocess.TimeoutExpired:
                recorder.kill()
                try:
                    recorder.communicate(timeout=3)
                except subprocess.TimeoutExpired:
                    pass
                cleanup_errors.append("Display recorder did not finish")
            except OSError as error:
                cleanup_errors.append(str(error))
        try:
            if fixture and fixture.poll() is None:
                try:
                    fixture_command("quit")
                except (RuntimeError, BrokenPipeError):
                    pass
                try:
                    fixture.wait(timeout=3)
                except subprocess.TimeoutExpired:
                    fixture.terminate()
                    fixture.wait(timeout=3)
            elif created and args.mode == "chrome" and not borrowed:
                close_owned_window(created)
            time.sleep(0.4)
            execute("workspace", "switch", str(active["index"]))
            if borrowed:
                execute("load-layout", str(backup), "--scope", "workspace")
                time.sleep(args.settle)
                report["after_restore_layout"] = query("layout")
                report["borrowed_layout_restored"] = (
                    layout_signature(report["before_layout"]["container_tree"])
                    == layout_signature(report["after_restore_layout"]["container_tree"]))
                if not report["borrowed_layout_restored"]:
                    cleanup_errors.append("Borrowed workspace column sizes or order were not restored")
            after = query("windows")
            if original and any(window["id"] == original["id"] for window in after):
                focus(original)
                time.sleep(0.3)
            if restore_pointer:
                cursor.move(original_cursor)
            report["after_windows"] = query("windows")
            report["original_window_ids_preserved"] = before_ids.issubset(
                {window["window_server_id"] for window in report["after_windows"]})
            if borrowed:
                report["borrowed_window_preserved"] = native_window_present(borrowed)
                target_preserved = report["borrowed_window_preserved"]
            else:
                if fixture is not None:
                    report["test_window_closed"] = fixture.poll() is not None
                elif created:
                    report["test_window_closed"] = not native_window_present(created)
                else:
                    report["test_window_closed"] = not report.get("chrome_launch_requested", False)
                target_preserved = report["test_window_closed"]
            if not report["original_window_ids_preserved"] or not target_preserved:
                cleanup_errors.append("Window inventory was not restored")
        except Exception as error:
            cleanup_errors.append(str(error))
        finally:
            if native:
                native.dispose()
            report["cleanup_errors"] = cleanup_errors
            (args.output / "report.json").write_text(json.dumps(report, indent=2) + "\n")
        if cleanup_errors:
            raise RuntimeError("; ".join(cleanup_errors))


def main():
    if len(sys.argv) == 3 and sys.argv[1] == "--fixture":
        content = module("wm_content_fixture", ROOT / "tests/rift-content-render-live-test.py")
        content.fixture(sys.argv[2])
        return
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--client", type=Path, default=ROOT / "target/debug/wm-cli")
    parser.add_argument("--mode", choices=["fixture", "chrome"], default="fixture")
    parser.add_argument("--window-id", type=int, help="Borrow an existing managed Chrome window and restore its saved Wm layout afterward.")
    parser.add_argument("--run", action="store_true", help="Run resize scenes on a disposable or borrowed window. Without this flag, inspect readiness only.")
    parser.add_argument("--output", type=Path, default=Path("/private/tmp/wm-live-resize-verification") / time.strftime("%Y%m%d-%H%M%S"))
    parser.add_argument("--trace-file", type=Path, help="JSONL emitted by the installed daemon via WM_LIVE_RESIZE_TRACE.")
    parser.add_argument("--video", action="store_true", help="Record the display region, including Wm's separate overlay.")
    parser.add_argument("--settle", type=float, default=3, help="Seconds to wait after the last command.")
    parser.add_argument("--fixture-bin", type=Path, help="Optional normal-level managed AppKit probe supporting the --hold JSON stdin protocol.")
    parser.add_argument("--qt-python", type=Path, default=Path("/Applications/Anki.app/Contents/Frameworks/Python.framework/Versions/3.13/bin/python3.13"))
    parser.add_argument("--qt-packages", type=Path, default=Path("/Applications/Anki.app/Contents/Resources/app_packages"))
    parser.add_argument("--chrome", type=Path, default=Path("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"))
    parser.add_argument("--url", default="https://x.com/seratch_ja")
    args = parser.parse_args()
    args.output = args.output.resolve()
    if args.trace_file:
        args.trace_file = args.trace_file.resolve()
    if args.window_id is not None and (args.mode != "chrome" or args.window_id <= 0):
        parser.error("--window-id requires --mode chrome and a positive native window ID")
    if args.run and not args.trace_file:
        parser.error("--run requires --trace-file so a fallback cannot be mistaken for production proxy behavior")
    if args.mode == "fixture" and not args.fixture_bin and (not args.qt_python.is_file() or not args.qt_packages.is_dir()):
        parser.error("Provide --fixture-bin or an installed PyQt6 runtime via --qt-python and --qt-packages")
    if args.mode == "fixture" and args.fixture_bin and not args.fixture_bin.is_file():
        parser.error("--fixture-bin does not exist")
    if args.mode == "chrome" and args.window_id is None and not args.chrome.is_file():
        parser.error("--chrome does not exist")
    if args.settle < 0.5:
        parser.error("--settle must be at least 0.5 seconds")
    run(args)


if __name__ == "__main__":
    main()
