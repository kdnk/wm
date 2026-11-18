#!/usr/bin/env python3
# Modified by kdnk for wm on 2026-11-15. Original attribution is preserved in LICENSE.
"""Check native hover focus for tiled windows and opt-in unmanaged dialogs."""

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


def helper(name, filename):
    spec = importlib.util.spec_from_file_location(name, Path(__file__).with_name(filename))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def check_unmanaged_dialog(pid, source, target, snapshot, move, query_windows, keyboard_open):
    title = source["title"].rsplit("-", 1)[0] + "-dialog"

    def wait_for(check, message):
        deadline = time.monotonic() + 4
        while True:
            observation = snapshot()
            if check(observation):
                return observation
            assert time.monotonic() < deadline, f"{message}: {observation}"
            time.sleep(0.04)

    def center(frame):
        return [frame[0] + frame[2] / 2, frame[1] + frame[3] / 2]

    def inside(point, frame):
        x, y, width, height = frame
        return x <= point[0] < x + width and y <= point[1] < y + height

    before = wait_for(lambda state: state["native"]["focused_window"] == source["title"]
                      and inside(state["pointer"], state["native"]["frames"][source["title"]]),
                      "Keyboard source did not receive focus and pointer")
    frames = before["native"]["frames"]
    assert not inside(before["pointer"], frames[target["title"]])
    requested = keyboard_open()["frames"][title]
    opened = wait_for(lambda state: state["native"]["focused_window"] == title
                      and inside(state["pointer"], state["native"]["frames"][title]),
                      "Pointer did not follow the keyboard-opened dialog")
    frame = opened["native"]["frames"][title]
    assert frame == requested, f"Dialog opened at {frame} instead of its app-chosen frame {requested}"
    assert not inside(before["pointer"], frame), "Pointer was already over the dialog"
    assert inside(center(frame), frames[target["title"]])
    assert abs(frame[2] - 220) <= 2 and 140 <= frame[3] <= 180, "Dialog lost its requested size"
    move(center(frames[source["title"]]))
    away = wait_for(lambda state: state["native"]["focused_window"] == source["title"],
                    "Hover did not refocus the source")
    move(center(frame))
    back = wait_for(lambda state: state["native"]["focused_window"] == title,
                    "Hover did not refocus the unmanaged dialog")
    for state in (opened, away, back):
        actual = state["native"]["frames"]
        assert all(actual[name] == expected for name, expected in frames.items()), "Managed windows moved"
        assert actual[title] == frame, "Dialog moved during hover"
    current = [window for window in query_windows() if window["id"]["pid"] == pid]
    assert {w["title"]: w["layout_position"] for w in current if w["layout_position"]} == {
        w["title"]: w["layout_position"] for w in (source, target)
    }, "Workspace columns changed after opening the unmanaged dialog"
    return {"before": before, "opened": opened, "hover_source": away, "hover_dialog": back}


def check_parent_following(command, execute, setting, query_windows, source, target, report):
    owner = source["title"]
    peer = target["title"]
    title = owner.rsplit("-", 1)[0] + "-dialog"
    start = command("snapshot")["frames"]
    offset = [start[title][i] - start[owner][i] for i in (0, 1)]
    size = start[title][2:]
    setting("settings.animation_duration", 0.3)
    setting("settings.animate", True)
    setting("settings.layout.scrolling.animate", True)

    def transition(name, operation):
        before = command("snapshot")["frames"]
        command("start")
        execute(*operation)
        time.sleep(0.85)
        frames = [s["frames"] for s in command("trace") if s["kind"] == "sample"]
        assert frames, f"No native frames sampled during {name}"
        after = frames[-1]
        compact = [{key: sample[window] for key, window in
                    (("owner", owner), ("peer", peer), ("dialog", title))} for sample in frames]
        report[name] = compact
        movement = after[owner][0] - before[owner][0]
        low, high = sorted((after[owner][0], before[owner][0]))
        interior = [s for s in frames if low + 2 < s[owner][0] < high - 2]

        def follows(sample):
            return all(abs(sample[title][i] - sample[owner][i] - offset[i]) <= 2 for i in (0, 1))

        assert all(s[title][2:] == size for s in frames), f"{name} resized the dialog"
        assert all(follows(s) for s in frames[-4:]), f"{name} lost the dialog offset after settling"
        if name in ("focus_left", "focus_right"):
            assert abs(movement) > 50, f"{name} did not move the editor: {movement}"
            assert any(follows(s) for s in interior), f"{name} dialog did not follow the moving editor"
        tiles = [w["title"] for w in query_windows() if w["id"]["pid"] == source["id"]["pid"]
                 and w["layout_position"]]
        assert sorted(tiles) == sorted((owner, peer)), f"{name} changed the tiled windows: {tiles}"
        return movement, after[peer][0] - before[peer][0]

    left, _ = transition("focus_left", ("window", "focus", "left"))
    right, _ = transition("focus_right", ("window", "focus", "right"))
    assert left > 0 and right < 0, "Directional focus did not pan the editor both ways"
    swap = ("layout", "swap-windows", json.dumps(source["id"]), json.dumps(target["id"]))
    owner_dx, peer_dx = transition("swap_editors", swap)
    assert abs(owner_dx - peer_dx) > 50, "Swap did not distinguish the editor from its same-app peer"


def run(args):
    motion = helper("wm_hover_motion", "motion-polish-live-test.py")
    safety = helper("wm_hover_safety", "rift-settings-focus-live-test.py")
    safety.require_unlocked()
    pointer = motion.Pointer()
    pointer.cg.CGEventSetTimestamp.argtypes = [ctypes.c_void_p, ctypes.c_uint64]
    system = ctypes.CDLL("/usr/lib/libSystem.B.dylib")
    system.mach_absolute_time.restype = ctypes.c_uint64

    class Timebase(ctypes.Structure):
        _fields_ = [("numer", ctypes.c_uint32), ("denom", ctypes.c_uint32)]

    timebase = Timebase()
    system.mach_timebase_info(ctypes.byref(timebase))
    client = str(args.client.expanduser().resolve())
    args.output.mkdir(parents=True)

    def query(kind):
        return json.loads(subprocess.check_output([client, "query", kind], timeout=5))

    def execute(*command):
        safety.require_unlocked()
        subprocess.run([client, "execute", *command], check=True, capture_output=True, timeout=5)

    def setting(key, value):
        execute("config", "set", key, json.dumps(value))

    config = json.loads(subprocess.check_output([client, "execute", "config", "get"], timeout=5))
    overrides = {
        "settings.animate": False,
        "settings.layout.scrolling.animate": False,
        "settings.mouse_follows_focus": args.unmanaged_dialog,
        "settings.mouse_hides_on_focus": False,
        "settings.focus_follows_mouse": True,
        "settings.layout.scrolling.preserve_window_sizes": False,
        "settings.layout.scrolling.column_width_ratio": 0.7 if args.follow_parent else 0.5,
    }
    original_settings = {}
    if args.follow_parent:
        overrides["settings.animation_duration"] = config["settings"]["animation_duration"]
    if args.unmanaged_dialog:
        assert any(rule.get("title_regex") == "^wm-hover-[0-9a-f]+-dialog$"
                   and rule.get("manage") is False and rule.get("track_focus") is True
                   and (not args.follow_parent or rule.get("follow_parent") is True)
                   for rule in config["virtual_workspaces"]["app_rules"]), (
            "Candidate configuration needs a ^wm-hover-[0-9a-f]+-dialog$ title rule "
            "with manage=false, track_focus=true and follow_parent=true when testing parent motion")
    for key in overrides:
        value = config
        for part in key.split("."):
            value = value[part]
        original_settings[key] = value
    original_windows = query("windows")
    original_focus = next((w for w in original_windows if w["is_focused"]), None)
    workspaces = query("workspaces")
    active = next(w for w in workspaces if w["is_focused"])
    empty = next((w for w in workspaces if w["display_uuid"] == active["display_uuid"]
                  and w["window_count"] == 0 and w["layout_mode"] == "scrolling"), None)
    assert empty is not None, "An empty scrolling workspace is required"
    display = next(d for d in query("displays") if d["uuid"] == active["display_uuid"])
    viewport = display["frame"]
    screen_left = viewport["origin"]["x"]
    screen_right = screen_left + viewport["size"]["width"]
    original_pointer = pointer.position()
    label = "wm-hover-" + uuid.uuid4().hex[:8]
    titles = {f"{label}-{i}" for i in range(2)}
    report = {"label": label, "settings": overrides, "trials": [], "failures": []}
    fixture = None
    backup = args.output / "layout-before.ron"
    execute("save-layout", str(backup.resolve()))

    def windows():
        return [w for w in query("windows") if w["id"]["pid"] == fixture.pid
                and w["title"] in titles]

    def focus(window):
        assert window["id"]["pid"] == fixture.pid and window["title"] in titles
        current = next(w for w in windows() if w["id"] == window["id"])
        assert current["title"] == window["title"], "Fixture identity changed"
        execute("window", "focus", "--window-id", json.dumps(window["id"]),
                "--window-server-id", str(window["window_server_id"]))

    def reply():
        assert select.select([fixture.stdout], [], [], 5)[0], "Fixture did not respond"
        line = fixture.stdout.readline()
        assert line, f"Fixture exited with {fixture.poll()}"
        return json.loads(line)

    def command(operation):
        fixture.stdin.write(json.dumps({"op": operation}) + "\n")
        fixture.stdin.flush()
        return reply()

    def snapshot():
        native = command("snapshot")
        current = windows()
        selected = next((w["title"] for w in current if w["is_focused"]), None)
        return {"native": native, "logical_focus": selected, "pointer": pointer.position()}

    def move(point, timestamp=None):
        event = pointer.cg.CGEventCreateMouseEvent(None, 5, motion.Point(*point), 0)
        assert event, "Cannot create native mouse motion"
        try:
            pointer.cg.CGEventSetFlags(event, 0)
            if timestamp is not None:
                pointer.cg.CGEventSetTimestamp(event, timestamp)
            pointer.cg.CGEventPost(0, event)
        finally:
            pointer.cf.CFRelease(event)

    def matches(observation, window):
        return (observation["native"]["focused_window"] == window["title"]
                and observation["logical_focus"] == window["title"])

    try:
        execute("workspace", "switch", str(empty["index"]))
        for key, value in overrides.items():
            setting(key, value)
        fixture = subprocess.Popen([
            str(args.qt_python), "-u", str(Path(__file__).with_name("rift-content-render-live-test.py")),
            "--fixture", label,
        ], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True,
            env=dict(os.environ, PYTHONPATH=str(args.qt_packages), WM_FIXTURE_WINDOW_COUNT="2",
                     WM_FIXTURE_FOCUS_DIALOG="1" if args.unmanaged_dialog else ""))
        assert reply()["pid"] == fixture.pid
        deadline = time.monotonic() + 10
        while len(windows()) != 2:
            assert time.monotonic() < deadline, "wm did not manage both fixture windows"
            time.sleep(0.1)
        ordered = sorted(windows(), key=lambda w: w["layout_position"]["column"])
        target, source = ordered
        report["source"] = source
        report["target"] = target
        if args.unmanaged_dialog:
            focus(source)
            # The previously focused window can already be the source on a fresh workspace.
            frame = command("snapshot")["frames"][source["title"]]
            move([frame[0] + frame[2] / 2, frame[1] + frame[3] / 2])
            report["dialog"] = check_unmanaged_dialog(
                fixture.pid, source, target, snapshot, move, lambda: query("windows"),
                lambda: command("dialog_key"))
            print("PASS unmanaged dialog retains native placement and follows keyboard and hover focus", flush=True)
            if args.follow_parent:
                report["parent_motion"] = {}
                check_parent_following(command, execute, setting, lambda: query("windows"),
                                       source, target, report["parent_motion"])
                print("PASS dialog follows its editor across horizontal navigation and a same-app window swap", flush=True)
            return
        for trial in range(args.trials):
            focus(source)
            deadline = time.monotonic() + 3
            while not matches(snapshot(), source):
                assert time.monotonic() < deadline, "Source did not acquire native and logical focus"
                time.sleep(0.025)
            time.sleep(0.15)
            native = command("snapshot")
            a, b = native["frames"][source["title"]], native["frames"][target["title"]]
            left = max(screen_left, b[0])
            boundary = min(screen_right, b[0] + b[2])
            assert boundary - left > 80 and a[0] <= boundary + 5 <= min(screen_right, a[0] + a[2]), (
                "Both native fixture windows must expose the shared boundary")
            y = max(a[1], b[1]) + min(a[3], b[3]) * 0.5
            start, end = [boundary + 5, y], [boundary - 5, y]
            move(start)
            time.sleep(0.08)
            before = snapshot()
            assert matches(before, source), "Source focus changed before the boundary crossing"
            first_timestamp = system.mach_absolute_time()
            move(start, first_timestamp)
            time.sleep(args.gap_ms / 1000)
            last_timestamp = first_timestamp + int(args.gap_ms * 1_000_000 * timebase.denom / timebase.numer)
            move(end, last_timestamp)
            time.sleep(0.25)
            stopped = snapshot()
            assert all(abs(a - b) < 1 for a, b in zip(stopped["pointer"], end)), (
                "Pointer moved after the final crossing; live result is inconclusive")
            for i in range(3):
                move([end[0] - i % 2, y])
                time.sleep(0.04)
            recovered = snapshot()
            assert matches(recovered, target), "Fresh motion failed to acquire target native focus"
            row = {"trial": trial, "start": start, "end": end,
                   "timestamp_gap_ms": args.gap_ms, "before": before,
                   "stopped": stopped, "recovered": recovered,
                   "stalled": not matches(stopped, target)}
            report["trials"].append(row)
            print(f"trial {trial}: stopped={stopped['native']['focused_window']} "
                  f"recovered={recovered['native']['focused_window']} stalled={row['stalled']}", flush=True)
        stalled = sum(row["stalled"] for row in report["trials"])
        assert stalled == 0, f"Final boundary motion lost native hover focus in {stalled}/{args.trials} trials"
        print(f"PASS native hover focus follows final boundary motion in {args.trials} trials", flush=True)
    except Exception as error:
        report["failures"].append(str(error))
        raise
    finally:
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
            try:
                for key, value in original_settings.items():
                    setting(key, value)
                execute("workspace", "switch", str(active["index"]))
                execute("load-layout", str(backup.resolve()), "--scope", "displays")
                if original_focus:
                    execute("window", "focus", "--window-id", json.dumps(original_focus["id"]),
                            "--window-server-id", str(original_focus["window_server_id"]))
                deadline = time.monotonic() + 5
                while True:
                    restored = {w["window_server_id"]: w for w in query("windows")}
                    report["layout_restored"] = all(
                        (actual := restored.get(w["window_server_id"])) is not None
                        and actual["layout_position"] == w["layout_position"]
                        and all(abs(actual["frame"]["size"][axis] - w["frame"]["size"][axis]) <= 1
                                for axis in ["width", "height"])
                        for w in original_windows)
                    if report["layout_restored"] or time.monotonic() >= deadline:
                        break
                    time.sleep(0.1)
                assert report["layout_restored"], "Original window order or sizes were not restored"
            finally:
                pointer.warp(original_pointer)
                (args.output / "report.json").write_text(json.dumps(report, indent=2) + "\n")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--client", type=Path, default=Path("~/.local/bin/wm"))
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--trials", type=int, default=5)
    parser.add_argument("--unmanaged-dialog", action="store_true",
                        help="Check a keyboard-opened modeless dialog with a track_focus rule")
    parser.add_argument("--follow-parent", action="store_true",
                        help="Also check dialog offset and size during horizontal workspace movement")
    parser.add_argument("--gap-ms", type=float, default=4.0)
    parser.add_argument("--qt-python", type=Path, default=Path("/Applications/Anki.app/Contents/Frameworks/Python.framework/Versions/3.13/bin/python3.13"))
    parser.add_argument("--qt-packages", type=Path, default=Path("/Applications/Anki.app/Contents/Resources/app_packages"))
    args = parser.parse_args()
    args.unmanaged_dialog |= args.follow_parent
    if args.trials < 1 or not 0 < args.gap_ms < 16:
        parser.error("trials must be positive and gap-ms must be between 0 and 16")
    run(args)
