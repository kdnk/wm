#!/usr/bin/env python3
# Modified by kdnk for wm on 2026-11-14. Original attribution is preserved in LICENSE.
"""Verify content presentation for every window in a simultaneous Wm resize."""

import argparse
from contextlib import ExitStack
import ctypes as C
import importlib.util
import json
import math
import os
from pathlib import Path
import plistlib
import select
import subprocess
import sys
import time
import uuid

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[1]


def module(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    loaded = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(loaded)
    return loaded


live = module("wm_single_resize_verifier", ROOT / "tests/rift-live-resize-live-test.py")


def traces_since(path, offset, window_ids):
    traces = {window_id: [] for window_id in window_ids}
    if not path or not path.is_file():
        return traces, offset
    with path.open("rb") as source:
        source.seek(offset)
        chunk = source.read()
    complete = chunk.rfind(b"\n") + 1
    for line in chunk[:complete].splitlines():
        event = json.loads(line)
        if event.get("window_id") in traces:
            traces[event["window_id"]].append(event)
    return traces, offset + complete


def size_changed(participant):
    return (len(participant["native_sizes"]) > 1
            or not live.near(participant["from"][2:], participant["final"][2:], 0.5))


def validate_participant(name, participant, command_count, axis, returns_to_start, require_motion=True):
    identifier = participant["window_server_id"]
    prefix = f"{name}: window {identifier}"
    samples = participant["native_samples"]
    sizes = participant["native_sizes"]
    assert samples, f"{prefix} has no native samples"
    assert not participant["native_read_errors"], f"{prefix} native sampling was incomplete"
    assert live.near(participant["final"], participant["target"]), f"{prefix} native frame differs from Wm target"
    assert len(sizes) - 1 <= command_count, f"{prefix} repeatedly reflowed through native sizes: {sizes}"
    if returns_to_start:
        assert live.near(participant["final"], participant["from"]), f"{prefix} did not return to its initial frame"
    if not require_motion:
        assert not size_changed(participant), f"{prefix} changed size without capture verification"
        return
    assert len(sizes) > 1, f"{prefix} never changed native size"
    if not returns_to_start:
        assert abs(participant["final"][axis] - participant["from"][axis]) > 2, f"{prefix} did not change on the requested axis"
    counts = participant["content_event_counts"]
    assert 0 < counts["resize"] <= command_count, f"{prefix} content resize count was {counts}"
    assert counts["paint_end"] > 0, f"{prefix} did not paint the changed layout"
    events = participant["proxy_trace"]
    phases = {event.get("phase") for event in events}
    assert "fallback" not in phases, f"{prefix} fell back to native resizing"
    assert "animating" in phases and "finished" in phases, f"{prefix} compositor did not complete: {sorted(phases)}"
    assert max((event.get("live_updates", 0) for event in events), default=0) > 0, f"{prefix} has no live target frames"


def validate_shared_presentation(name, participants):
    groups = []
    for participant in participants:
        starts = set()
        for event in participant["proxy_trace"]:
            if event.get("phase") != "animating":
                continue
            cohort, started = event.get("cohort_id"), event.get("presentation_started_us")
            assert cohort is not None and isinstance(started, int) and started > 0, (
                f"{name}: window {participant['window_server_id']} has no cohort presentation clock")
            starts.add((cohort, started))
        assert starts, f"{name}: window {participant['window_server_id']} has no shared presentation"
        groups.append(starts)
    assert all(group == groups[0] for group in groups[1:]), f"{name}: participants have different cohorts or presentation clocks: {groups}"
    return [{"cohort_id": cohort, "presentation_started_us": started} for cohort, started in sorted(groups[0])]


def native_window_ids():
    cg = C.CDLL("/System/Library/Frameworks/CoreGraphics.framework/CoreGraphics")
    cf = C.CDLL("/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation")
    cg.CGWindowListCopyWindowInfo.argtypes, cg.CGWindowListCopyWindowInfo.restype = [C.c_uint32, C.c_uint32], C.c_void_p
    for name, result, arguments in [
        ("CFPropertyListCreateData", C.c_void_p, [C.c_void_p, C.c_void_p, C.c_long, C.c_ulong, C.c_void_p]),
        ("CFDataGetLength", C.c_long, [C.c_void_p]),
        ("CFDataGetBytePtr", C.c_void_p, [C.c_void_p]),
        ("CFRelease", None, [C.c_void_p]),
    ]:
        function = getattr(cf, name)
        function.restype, function.argtypes = result, arguments
    windows = cg.CGWindowListCopyWindowInfo(0, 0)
    if not windows:
        raise RuntimeError("Native WindowServer inventory is unavailable")
    try:
        data = cf.CFPropertyListCreateData(None, windows, 200, 0, None)
        if not data:
            raise RuntimeError("Native WindowServer inventory cannot be decoded")
        try:
            values = plistlib.loads(C.string_at(cf.CFDataGetBytePtr(data), cf.CFDataGetLength(data)))
        finally:
            cf.CFRelease(data)
    finally:
        cf.CFRelease(windows)
    return {window["kCGWindowNumber"] for window in values}


def fixture_windows_closed(fixtures, inventory=native_window_ids):
    identifiers = {fixture.window["window_server_id"] for fixture in fixtures if fixture.window is not None}
    return not identifiers or identifiers.isdisjoint(inventory())


def wait_workspace(query, index, required_ids=(), timeout=10):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        workspace = next((workspace for workspace in query("workspaces") if workspace["is_focused"]), None)
        if (workspace is not None and workspace["index"] == index
                and set(required_ids).issubset({window["window_server_id"] for window in workspace["windows"]})):
            return workspace
        time.sleep(0.05)
    raise RuntimeError(f"Workspace {index} did not become active with its expected native windows")


def wait_owned_removed(query, identifiers, timeout=10):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        present = {window["window_server_id"] for workspace in query("workspaces") for window in workspace["windows"]}
        if set(identifiers).isdisjoint(present):
            return
        time.sleep(0.05)
    raise RuntimeError("Exited fixture windows remain in Wm's workspace inventory")


class Fixture:
    def __init__(self, args, label):
        self.label = label
        self.window = self.native = None
        self.stderr = (args.output / (label + ".stderr")).open("w")
        try:
            self.process = subprocess.Popen(
                [str(args.qt_python), "-u", str(Path(__file__).resolve()), "--fixture", label],
                stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self.stderr, text=True,
                env=dict(os.environ, PYTHONPATH=str(args.qt_packages.resolve()), WM_FIXTURE_WINDOW_COUNT="1"),
            )
        except Exception:
            self.stderr.close()
            raise
        self.pid = None

    def reply(self):
        if not select.select([self.process.stdout], [], [], 6)[0]:
            raise RuntimeError(f"Fixture {self.label} response timed out")
        line = self.process.stdout.readline()
        if not line:
            raise RuntimeError(f"Fixture {self.label} exited with status {self.process.poll()}")
        return json.loads(line)

    def command(self, operation):
        self.process.stdin.write(json.dumps({"op": operation}) + "\n")
        self.process.stdin.flush()
        return self.reply()

    def close(self):
        try:
            if self.process.poll() is None:
                try:
                    self.process.stdin.write('{"op":"quit"}\n')
                    self.process.stdin.flush()
                    self.process.wait(timeout=3)
                except (BrokenPipeError, subprocess.TimeoutExpired):
                    self.process.terminate()
                    try:
                        self.process.wait(timeout=3)
                    except subprocess.TimeoutExpired:
                        self.process.kill()
                        self.process.wait(timeout=3)
        finally:
            if self.native:
                self.native.dispose()
                self.native = None
            self.stderr.close()


def run(args):
    guard = module("wm_multi_focus_guard", ROOT / "tests/rift-settings-focus-live-test.py")
    guard.require_unlocked()
    client = str(args.client.resolve())

    def query(kind):
        return json.loads(subprocess.check_output([client, "query", kind], timeout=5))

    def execute(*command):
        guard.require_unlocked()
        subprocess.run([client, "execute", *command], check=True, timeout=5, stdout=subprocess.DEVNULL)

    before = query("windows")
    before_ids = {window["window_server_id"] for window in before}
    original = next((window for window in before if window["is_focused"]), None)
    workspaces = query("workspaces")
    active = next(workspace for workspace in workspaces if workspace["is_focused"])
    empty = next((workspace for workspace in workspaces
                  if workspace["display_uuid"] == active["display_uuid"] and workspace["window_count"] == 0
                  and workspace["layout_mode"] == "scrolling"), None)
    if empty is None:
        raise RuntimeError("An empty scrolling workspace on the focused display is required")
    display = next(display for display in query("displays") if display["uuid"] == active["display_uuid"])
    readiness = {"workspace": empty, "display": display, "trace_file": str(args.trace_file) if args.trace_file else None,
                 "separate_processes": True, "window_count": args.windows,
                 "would_run": ["column-50-to-100", "column-100-to-50", "row-height", "row-height-reversal"],
                 "visual_quality": "Inspect display recordings of every participant."}
    if not args.run:
        print(json.dumps({"ready": True, "mutations": False, **readiness}, indent=2))
        return

    args.output.mkdir(parents=True, exist_ok=False)
    report = {**readiness, "before_windows": before, "original_workspace": active,
              "before_layout": query("layout"), "scenes": [], "failures": []}
    backup = args.output / "before-layout.ron"
    execute("save-layout", str(backup))
    fixtures = []
    source = recorder = cursor = original_cursor = cursor_target = None
    capture_rect = ",".join(str(math.ceil(value)) for value in live.rect(display["frame"]))

    def owned_windows():
        pids = {fixture.pid for fixture in fixtures if fixture.pid is not None}
        return [window for window in query("windows") if window["id"]["pid"] in pids]

    def focus(window):
        execute("window", "focus", "--window-id", json.dumps(window["id"]),
                "--window-server-id", str(window["window_server_id"]))
        time.sleep(0.08)
        if live.frontmost_pid() != window["id"]["pid"]:
            workspace = next(workspace for workspace in query("workspaces") if workspace["is_focused"])
            alternatives = [candidate for candidate in workspace["windows"]
                            if candidate["id"]["pid"] != window["id"]["pid"] and not candidate["is_floating"]]
            if alternatives:
                alternative = alternatives[0]
                execute("window", "focus", "--window-id", json.dumps(alternative["id"]),
                        "--window-server-id", str(alternative["window_server_id"]))
                execute("window", "focus", "--window-id", json.dumps(window["id"]),
                        "--window-server-id", str(window["window_server_id"]))
                time.sleep(0.15)

    def target_context(stage):
        workspace = next((workspace for workspace in query("workspaces") if workspace["is_focused"]), None)
        if workspace is None or workspace["index"] != empty["index"]:
            raise RuntimeError(f"{stage}: focus left the disposable workspace")
        expected = {fixture.window["window_server_id"] for fixture in fixtures}
        current = {window["window_server_id"]: window for window in workspace["windows"]
                   if window["window_server_id"] in expected}
        if set(current) != expected:
            raise RuntimeError(f"{stage}: a fixture window left the managed workspace")
        if any(window["is_floating"] for window in current.values()):
            raise RuntimeError(f"{stage}: a fixture became floating")
        if live.frontmost_pid() != source.pid or source.native.focused_window_id() != source.window["window_server_id"]:
            raise RuntimeError(f"{stage}: native focus left the selected fixture")
        return current

    try:
        cursor = live.Cursor()
        original_cursor = cursor.position()
        execute("workspace", "switch", str(empty["index"]))
        wait_workspace(query, empty["index"])
        label = "wm-multi-" + uuid.uuid4().hex[:8]
        for index in range(args.windows):
            fixture = Fixture(args, f"{label}-{index}")
            fixtures.append(fixture)
            fixture.pid = fixture.reply()["pid"]
        assert len({fixture.pid for fixture in fixtures}) == args.windows, "Every participant must run in a separate native process"
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            current = owned_windows()
            if len(current) == args.windows:
                break
            time.sleep(0.1)
        else:
            raise RuntimeError("Wm did not admit every disposable fixture window")
        for fixture in fixtures:
            candidates = [window for window in current if window["id"]["pid"] == fixture.pid]
            assert len(candidates) == 1, "A fixture process must own exactly one managed window"
            fixture.window = candidates[0]
            fixture.native = live.NativeWindow(fixture.pid, fixture.window["window_server_id"])
        source = min(fixtures, key=lambda fixture: fixture.window["layout_position"]["column"])
        for _ in range(args.windows - 1):
            current = owned_windows()
            source_column = next(window["layout_position"]["column"] for window in current
                                 if window["window_server_id"] == source.window["window_server_id"])
            separate = [window for window in current if window["layout_position"]["column"] != source_column]
            if not separate:
                break
            peer = min(separate, key=lambda window: abs(window["layout_position"]["column"] - source_column))
            direction = "left" if peer["layout_position"]["column"] > source_column else "right"
            source = next(fixture for fixture in fixtures
                          if fixture.window["window_server_id"] == peer["window_server_id"])
            focus(source.window)
            target_context("join setup")
            execute("layout", "join-window", direction)
            time.sleep(args.settle)
        joined = owned_windows()
        assert len(joined) == args.windows and len({window["layout_position"]["column"] for window in joined}) == 1, "Every fixture must share one column"
        source_id = min(joined, key=lambda window: window["layout_position"]["row"])["window_server_id"]
        source = next(fixture for fixture in fixtures if fixture.window["window_server_id"] == source_id)
        focus(source.window)
        width = live.rect(display["frame"])[2]
        for _ in range(3):
            if all(abs(fixture.native.frame()[2] - width * 0.5) <= 2 for fixture in fixtures):
                break
            target_context("half-width setup")
            execute(*live.PRESET)
            time.sleep(args.settle)
        else:
            raise RuntimeError("Every fixture row must reach half width")
        frame = source.native.frame()
        cursor_target = [frame[0] + min(500, frame[2] / 2), frame[1] + 12]
        cursor.move(cursor_target)
        report["participants"] = [fixture.window for fixture in fixtures]
        report["source_window_id"] = source.window["window_server_id"]

        def scene(name, operations, axis, returns_to_start=False, require_all=False):
            nonlocal recorder
            guard.require_unlocked()
            focus(source.window)
            cursor.move(cursor_target)
            target_context(name)
            initial = {fixture.window["window_server_id"]: fixture.native.frame() for fixture in fixtures}
            offset = args.trace_file.stat().st_size if args.trace_file.exists() else 0
            for fixture in fixtures:
                fixture.command("start")
            video_started_us = None
            if args.video:
                video_started_us = time.time_ns() // 1000
                recorder = subprocess.Popen([
                    "/usr/sbin/screencapture", "-x", "-v", "-V", "8", "-R", capture_rect,
                    str(args.output / (name + ".mov")),
                ], stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
                time.sleep(0.45)
                focus(source.window)
                cursor.move(cursor_target)
                target_context(name + " recording setup")
            commands = []
            with ExitStack() as stack:
                samplers = {fixture.window["window_server_id"]: stack.enter_context(live.Sampler(fixture.native))
                            for fixture in fixtures}
                time.sleep(0.03)
                for index, (delay, operation) in enumerate(operations):
                    if returns_to_start and index:
                        deadline = time.monotonic() + 4
                        while time.monotonic() < deadline:
                            traces, _ = traces_since(args.trace_file, offset, initial)
                            changing = {identifier for identifier, sampler in samplers.items()
                                        if len(live.summarize_sizes(sampler.records)) > 1}
                            animating = {identifier for identifier, events in traces.items()
                                         if any(event.get("phase") == "animating" for event in events)}
                            if len(changing) >= 2 and changing.issubset(animating):
                                break
                            time.sleep(0.005)
                        else:
                            raise RuntimeError(f"{name}: fewer than two size-changing participants began compositor motion")
                    time.sleep(delay)
                    target_context(name)
                    commands.append({"time_ns": time.monotonic_ns(), "command": operation})
                    execute(*operation)
                time.sleep(args.settle)
            app_traces = {fixture.pid: fixture.command("trace") for fixture in fixtures}
            current = target_context(name)
            traces, _ = traces_since(args.trace_file, offset, initial)
            participants = []
            for fixture in fixtures:
                identifier = fixture.window["window_server_id"]
                sampler = samplers[identifier]
                participants.append({
                    "window_server_id": identifier, "pid": fixture.pid, "title": fixture.window["title"],
                    "was_foreground": fixture.pid == source.pid, "from": initial[identifier],
                    "final": fixture.native.frame(), "target": live.rect(current[identifier]["frame"]),
                    "native_samples": sampler.records, "native_read_errors": sampler.errors,
                    "native_sizes": live.summarize_sizes(sampler.records), "app_trace": app_traces[fixture.pid],
                    "content_event_counts": live.paint_counts(app_traces[fixture.pid], fixture.window["title"]),
                    "proxy_trace": traces[identifier],
                })
            video = None
            if recorder:
                _, stderr = recorder.communicate(timeout=10)
                video = {"path": str(args.output / (name + ".mov")), "started_us": video_started_us,
                         "returncode": recorder.returncode, "stderr": stderr.decode(errors="replace")}
                recorder = None
            starts = [next((event["timestamp_us"] for event in participant["proxy_trace"]
                            if event.get("phase") == "animating"), None) for participant in participants]
            record = {"name": name, "commands": commands, "participants": participants, "video": video,
                      "animating_event_spread_us": max(starts) - min(starts) if all(start is not None for start in starts) else None,
                      "native_frontmost_pid_after": live.frontmost_pid(),
                      "native_focused_window_after": source.native.focused_window_id()}
            report["scenes"].append(record)
            (args.output / (name + ".json")).write_text(json.dumps(record, indent=2) + "\n")
            assert record["native_frontmost_pid_after"] == source.pid, f"{name}: frontmost application changed"
            assert record["native_focused_window_after"] == source.window["window_server_id"], f"{name}: native focus changed"
            changing = [participant for participant in participants if size_changed(participant)]
            record["size_changing_window_ids"] = [participant["window_server_id"] for participant in changing]
            assert len(changing) >= 2, f"{name}: fewer than two participants changed size"
            if require_all:
                assert len(changing) == args.windows, f"{name}: a column resize did not change every participant"
            for participant in participants:
                participant["size_changed"] = size_changed(participant)
                validate_participant(name, participant, len(operations), axis, returns_to_start,
                                     require_motion=participant["size_changed"])
            record["presentation_groups"] = validate_shared_presentation(name, changing)
            (args.output / (name + ".json")).write_text(json.dumps(record, indent=2) + "\n")
            if video:
                assert video["returncode"] == 0, f"{name}: display recording failed: {video['stderr']}"
            print(json.dumps({"scene": name, "participants": [
                {"window_id": participant["window_server_id"], "foreground": participant["was_foreground"],
                 "native_size_changes": len(participant["native_sizes"]) - 1,
                 "content_events": participant["content_event_counts"]} for participant in participants],
                "animating_event_spread_us": record["animating_event_spread_us"],
                "presentation_groups": record["presentation_groups"]}), flush=True)

        scene("column-50-to-100", [(0, live.PRESET)], 2, require_all=True)
        scene("column-100-to-50", [(0, live.PRESET)], 2, require_all=True)
        grow = ["window", "resize-grow", "--orientation", "vertical"]
        shrink = ["window", "resize-shrink", "--orientation", "vertical"]
        scene("row-height", [(0, grow)], 3)
        scene("row-height-reversal", [(0, grow), (0.045, shrink)], 3, returns_to_start=True)
    except Exception as error:
        report["failures"].append(f"{type(error).__name__}: {error}")
        report["failure_state"] = {}
        for kind in ["workspaces", "windows", "layout"]:
            try:
                report["failure_state"][kind] = query(kind)
            except Exception as query_error:
                report["failure_state"][kind] = {"error": str(query_error)}
        raise
    finally:
        cleanup_errors = []
        restore_pointer = False
        if cursor is not None and cursor_target is not None:
            try:
                restore_pointer = live.near(cursor.position(), cursor_target)
            except RuntimeError as error:
                cleanup_errors.append(str(error))
        if recorder:
            try:
                _, error = recorder.communicate(timeout=10)
                report["interrupted_video"] = {"path": recorder.args[-1], "returncode": recorder.returncode,
                                               "stderr": error.decode(errors="replace")}
            except subprocess.TimeoutExpired:
                recorder.kill()
                recorder.communicate(timeout=3)
                cleanup_errors.append("Display recorder did not finish")
            except OSError as error:
                cleanup_errors.append(str(error))
        for fixture in fixtures:
            try:
                fixture.close()
            except Exception as error:
                cleanup_errors.append(f"Fixture cleanup failed: {error}")
        try:
            owned_ids = {fixture.window["window_server_id"] for fixture in fixtures if fixture.window is not None}
            if owned_ids:
                wait_owned_removed(query, owned_ids)
            execute("workspace", "switch", str(active["index"]))
            active_ids = {window["window_server_id"] for window in active["windows"]}
            wait_workspace(query, active["index"], active_ids)
            execute("load-layout", str(backup), "--scope", "workspace")
            wait_workspace(query, active["index"], active_ids)
            time.sleep(args.settle)
            after = query("windows")
            report["original_focus_restored"] = original is None
            if original and any(window["id"] == original["id"] for window in after):
                focus(original)
                original_native = live.NativeWindow(original["id"]["pid"], original["window_server_id"])
                try:
                    report["original_focus_restored"] = (
                        live.frontmost_pid() == original["id"]["pid"]
                        and original_native.focused_window_id() == original["window_server_id"])
                finally:
                    original_native.dispose()
            if restore_pointer:
                cursor.move(original_cursor)
            report["after_workspace"] = wait_workspace(query, active["index"], active_ids)
            report["original_workspace_restored"] = report["after_workspace"]["index"] == active["index"]
            report["after_windows"] = query("windows")
            report["after_layout"] = query("layout")
            report["original_window_ids_preserved"] = before_ids.issubset(
                {window["window_server_id"] for window in report["after_windows"]})
            report["original_layout_restored"] = (live.layout_signature(report["before_layout"]["container_tree"])
                                                   == live.layout_signature(report["after_layout"]["container_tree"]))
            report["test_processes_closed"] = all(fixture.process.poll() is not None for fixture in fixtures)
            report["test_native_windows_closed"] = fixture_windows_closed(fixtures)
            if not all(report[key] for key in ["original_window_ids_preserved", "original_layout_restored", "original_focus_restored", "original_workspace_restored",
                                              "test_processes_closed", "test_native_windows_closed"]):
                cleanup_errors.append("Original window layout or disposable fixture inventory was not restored")
        except Exception as error:
            cleanup_errors.append(str(error))
        report["cleanup_errors"] = cleanup_errors
        (args.output / "report.json").write_text(json.dumps(report, indent=2) + "\n")
        if cleanup_errors:
            raise RuntimeError("; ".join(cleanup_errors))


def main():
    if len(sys.argv) == 3 and sys.argv[1] == "--fixture":
        content = module("wm_multi_content_fixture", ROOT / "tests/rift-content-render-live-test.py")
        content.fixture(sys.argv[2])
        return
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--client", type=Path, default=ROOT / "target/debug/wm")
    parser.add_argument("--windows", type=int, default=2, help="Number of disposable fixture processes, from 2 to 4.")
    parser.add_argument("--run", action="store_true", help="Create disposable windows and run resize scenes. Without this flag, inspect readiness only.")
    parser.add_argument("--output", type=Path, default=Path("/private/tmp/wm-multi-resize-verification") / time.strftime("%Y%m%d-%H%M%S"))
    parser.add_argument("--trace-file", type=Path, help="JSONL emitted by the installed daemon via WM_LIVE_RESIZE_TRACE.")
    parser.add_argument("--video", action="store_true", help="Record the display region containing every window and its overlay.")
    parser.add_argument("--settle", type=float, default=3)
    parser.add_argument("--qt-python", type=Path, default=Path("/Applications/Anki.app/Contents/Frameworks/Python.framework/Versions/3.13/bin/python3.13"))
    parser.add_argument("--qt-packages", type=Path, default=Path("/Applications/Anki.app/Contents/Resources/app_packages"))
    args = parser.parse_args()
    args.output = args.output.resolve()
    if args.trace_file:
        args.trace_file = args.trace_file.resolve()
    if not 2 <= args.windows <= 4:
        parser.error("--windows must be between 2 and 4")
    if args.run and not args.trace_file:
        parser.error("--run requires --trace-file so native fallback cannot be mistaken for composited content")
    if not args.qt_python.is_file() or not args.qt_packages.is_dir():
        parser.error("Provide an installed PyQt6 runtime via --qt-python and --qt-packages")
    if args.settle < 0.5:
        parser.error("--settle must be at least 0.5 seconds")
    run(args)


if __name__ == "__main__":
    main()
