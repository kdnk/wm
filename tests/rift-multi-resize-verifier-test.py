#!/usr/bin/env python3
# Modified by kdnk for wm on 2026-11-14. Original attribution is preserved in LICENSE.
from contextlib import redirect_stdout
import copy
import importlib.util
import io
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import time
from types import SimpleNamespace

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("wm_multi_verifier", ROOT / "tests/rift-multi-resize-live-test.py")
verifier = importlib.util.module_from_spec(spec)
spec.loader.exec_module(verifier)


def rejected(action, expected):
    try:
        action()
    except (AssertionError, RuntimeError) as error:
        assert expected in str(error), str(error)
    else:
        raise AssertionError(f"Verification accepted {expected}")


participant = {
    "window_server_id": 11, "from": [0, 45, 1024, 643], "final": [0, 45, 2048, 643], "target": [0, 45, 2048, 643],
    "native_samples": [{"frame": [0, 45, 1024, 643]}, {"frame": [0, 45, 2048, 643]}], "native_read_errors": [],
    "native_sizes": [[1024, 643], [2048, 643]], "content_event_counts": {"resize": 1, "paint_end": 1},
    "proxy_trace": [{"phase": "animating", "live_updates": 1, "cohort_id": 42, "presentation_started_us": 100},
                    {"phase": "finished", "live_updates": 18}],
}
verifier.validate_participant("grow", participant, 1, 2, False)
for changes, expected in [
    ({"native_sizes": [[width, 643] for width in range(1024, 2049, 64)]}, "repeatedly reflowed"),
    ({"proxy_trace": [{"phase": "fallback"}]}, "fell back to native resizing"),
    ({"proxy_trace": participant["proxy_trace"] + [{"phase": "fallback"}]}, "fell back to native resizing"),
    ({"content_event_counts": {"resize": 1, "paint_end": 0}}, "did not paint"),
    ({"target": [0, 45, 1024, 643]}, "differs from Wm target"),
]:
    broken = copy.deepcopy(participant)
    broken.update(changes)
    rejected(lambda: verifier.validate_participant("grow", broken, 1, 2, False), expected)
peer = copy.deepcopy(participant)
peer["window_server_id"] = 12
verifier.validate_shared_presentation("grow", [participant, peer])
for key, value, expected in [("cohort_id", 43, "different cohorts"),
                             ("presentation_started_us", 101, "different cohorts"),
                             ("presentation_started_us", None, "no cohort presentation clock")]:
    broken = copy.deepcopy(peer)
    broken["proxy_trace"][0][key] = value
    rejected(lambda: verifier.validate_shared_presentation("grow", [participant, broken]), expected)
print("PASS both participants need completed capture, native geometry, painting, and the same cohort clock")

unchanged = copy.deepcopy(peer)
unchanged.update({"window_server_id": 13, "final": [0, 752, 1024, 643], "target": [0, 752, 1024, 643],
                  "native_sizes": [[1024, 643]], "content_event_counts": {"resize": 0, "paint_end": 0},
                  "proxy_trace": []})
assert not verifier.size_changed(unchanged)
verifier.validate_participant("row", unchanged, 1, 3, False, require_motion=False)
changing = [window for window in [participant, peer, unchanged] if verifier.size_changed(window)]
assert len(changing) == 2
verifier.validate_shared_presentation("row", changing)
reversal = copy.deepcopy(participant)
reversal.update({"final": participant["from"], "target": participant["from"],
                 "native_sizes": [[1024, 643], [2048, 643], [1024, 643]],
                 "content_event_counts": {"resize": 2, "paint_end": 2}})
assert verifier.size_changed(reversal)
verifier.validate_participant("reversal", reversal, 2, 2, True)
print("PASS unchanged third windows need no capture, while reversals still require compositor verification")

with tempfile.TemporaryDirectory(prefix="wm-multi-verifier-") as directory:
    path = Path(directory) / "trace.jsonl"
    first = b'{"window_id":11,"phase":"animating"}\n'
    second = b'{"window_id":12,"phase":"animating"}\n'
    partial = b'{"window_id":11'
    path.write_bytes(first + second + partial)
    traces, offset = verifier.traces_since(path, 0, [11, 12])
    assert offset == len(first + second) and all(len(events) == 1 for events in traces.values())
    path.write_bytes(first + second + partial + b',"phase":"finished"}\n')
    tail, offset = verifier.traces_since(path, offset, [11, 12])
    assert tail[11][0]["phase"] == "finished" and not tail[12]
print("PASS one trace offset retains both participants and incomplete trailing lines")

dead_child = SimpleNamespace(window={"window_server_id": 88}, process=SimpleNamespace(poll=lambda: 0))
verifier.live.native_window_present = lambda _: (_ for _ in ()).throw(RuntimeError("Native AXWindows error -25204"))
assert dead_child.process.poll() == 0
assert verifier.fixture_windows_closed([dead_child], inventory=lambda: {99})
assert not verifier.fixture_windows_closed([dead_child], inventory=lambda: {88, 99})
rejected(lambda: verifier.fixture_windows_closed([dead_child], inventory=lambda: (_ for _ in ()).throw(RuntimeError("inventory unavailable"))),
         "inventory unavailable")
print("PASS exited child cleanup uses native window absence without swallowing inventory errors")

reads = {"workspace": 0, "removal": 0}


def delayed_workspace(kind):
    assert kind == "workspaces"
    reads["workspace"] += 1
    return [{"index": 1 if reads["workspace"] >= 2 else 2, "is_focused": True,
             "windows": [{"window_server_id": 99}] if reads["workspace"] >= 3 else []}]


def delayed_removal(kind):
    assert kind == "workspaces"
    reads["removal"] += 1
    return [{"windows": [{"window_server_id": 88}] if reads["removal"] < 3 else []}]


assert verifier.wait_workspace(delayed_workspace, 1, {99}, timeout=1)["index"] == 1
verifier.wait_owned_removed(delayed_removal, {88}, timeout=1)
assert reads == {"workspace": 3, "removal": 3}
for count in [1, 5]:
    result = subprocess.run([sys.executable, str(ROOT / "tests/rift-multi-resize-live-test.py"), "--windows", str(count)],
                            capture_output=True, text=True)
    assert result.returncode == 2 and "--windows must be between 2 and 4" in result.stderr
print("PASS workspace restoration waits for actual membership and window counts are bounded")

original = {"window_server_id": 99, "id": {"pid": 999, "idx": 99}, "is_focused": True, "is_floating": False,
            "layout_position": {"column": 0, "row": 0},
            "frame": {"origin": {"x": 0, "y": 45}, "size": {"width": 1024, "height": 1285}}}
layout = {"container_tree": {"node_type": "container", "frame": {"size": {"width": 1024, "height": 1285}}, "children": []}}
state = {"workspace": 1}
commands, instances = [], []


class FakeCursor:
    def position(self):
        return [10, 20]

    def move(self, point):
        pass


class FakeNativeWindow:
    def __init__(self, pid, identifier):
        self.identifier = identifier

    def focused_window_id(self):
        return self.identifier

    def dispose(self):
        pass


class FailedFixture:
    def __init__(self, args, label):
        self.pid = self.window = self.native = None
        self.closed = False
        self.process = SimpleNamespace(poll=lambda: 1 if self.closed else None)
        instances.append(self)

    def reply(self):
        raise RuntimeError("deliberate fixture startup failure")

    def close(self):
        self.closed = True


def query(arguments, **kwargs):
    kind = arguments[-1]
    if kind == "windows":
        value = [original] if state["workspace"] == 1 else []
    elif kind == "layout":
        value = layout
    elif kind == "displays":
        value = [{"uuid": "display", "frame": original["frame"]}]
    elif kind == "workspaces":
        if "switch_pending" in state:
            state["polls"] += 1
            if state["polls"] >= 3:
                state["workspace"] = state.pop("switch_pending")
        value = [{"index": 1, "is_focused": state["workspace"] == 1, "display_uuid": "display", "window_count": 1,
                  "layout_mode": "scrolling", "windows": [original]},
                 {"index": 2, "is_focused": state["workspace"] == 2, "display_uuid": "display", "window_count": 0,
                  "layout_mode": "scrolling", "windows": []}]
    else:
        raise AssertionError(arguments)
    return json.dumps(value).encode()


def execute(arguments, **kwargs):
    commands.append(arguments[2:])
    if arguments[2:4] == ["workspace", "switch"]:
        state["switch_pending"] = int(arguments[4])
        state["polls"] = 0
    if arguments[2] == "load-layout":
        assert state["workspace"] == 1 and "switch_pending" not in state, "Layout restore ran on the wrong workspace"
    return SimpleNamespace(returncode=0)


verifier.Fixture = FailedFixture
verifier.live.Cursor = FakeCursor
verifier.live.NativeWindow = FakeNativeWindow
verifier.live.frontmost_pid = lambda: 999
verifier.module = lambda *args: SimpleNamespace(require_unlocked=lambda: None)
verifier.time = SimpleNamespace(sleep=lambda _: None, monotonic=time.monotonic, monotonic_ns=time.monotonic_ns)
verifier.subprocess = SimpleNamespace(check_output=query, run=execute, TimeoutExpired=subprocess.TimeoutExpired,
                                     DEVNULL=subprocess.DEVNULL)
with tempfile.TemporaryDirectory(prefix="wm-multi-cleanup-") as directory:
    args = SimpleNamespace(client=ROOT / "target/debug/wm-cli", run=False, output=Path(directory) / "result",
                           trace_file=Path(directory) / "trace.jsonl", video=False, settle=0.5, windows=2)
    with redirect_stdout(io.StringIO()):
        verifier.run(args)
    assert not commands and not args.output.exists(), "Readiness must not change the desktop or create artifacts"
    args.run = True
    rejected(lambda: verifier.run(args), "deliberate fixture startup failure")
    report = json.loads((args.output / "report.json").read_text())
    assert all(fixture.closed for fixture in instances)
    assert not report["cleanup_errors"], report["cleanup_errors"]
    assert all(report[key] for key in ["original_layout_restored", "original_window_ids_preserved",
                                       "original_focus_restored", "original_workspace_restored", "test_processes_closed"])
    assert ["workspace", "switch", "1"] in commands
    assert any(command[:1] == ["load-layout"] for command in commands)
    assert not any(command[:2] in [["window", "close"], ["workspace", "move-window"]] for command in commands)
print("PASS readiness is read-only and failed startup closes owned children while restoring the user's workspace and layout")
