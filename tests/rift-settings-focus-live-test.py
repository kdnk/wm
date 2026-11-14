#!/usr/bin/env python3
# Modified by kdnk for wm on 2026-11-14. Original attribution is preserved in LICENSE.
import argparse
import ctypes
import json
from pathlib import Path
import subprocess
import time

PROFILES = {
    "muse": {"name": "Muse", "bundle": "com.meta.endo", "settings": "Settings", "menu": "Settings"},
    "anki": {"name": "Anki", "bundle": "net.ankiweb.anki", "settings": "Preferences", "menu": "Preferences..."},
}


def applescript(source):
    return subprocess.check_output(["osascript", "-e", source], text=True, timeout=4).strip()


def current_session_is_locked():
    quartz = ctypes.CDLL("/System/Library/Frameworks/CoreGraphics.framework/CoreGraphics")
    core = ctypes.CDLL("/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation")
    quartz.CGSessionCopyCurrentDictionary.argtypes = []
    quartz.CGSessionCopyCurrentDictionary.restype = ctypes.c_void_p
    core.CFStringCreateWithCString.argtypes = [ctypes.c_void_p, ctypes.c_char_p, ctypes.c_uint32]
    core.CFStringCreateWithCString.restype = ctypes.c_void_p
    core.CFDictionaryGetValue.argtypes = [ctypes.c_void_p, ctypes.c_void_p]
    core.CFDictionaryGetValue.restype = ctypes.c_void_p
    core.CFGetTypeID.argtypes = [ctypes.c_void_p]
    core.CFGetTypeID.restype = ctypes.c_ulong
    core.CFBooleanGetTypeID.argtypes = []
    core.CFBooleanGetTypeID.restype = ctypes.c_ulong
    core.CFBooleanGetValue.argtypes = [ctypes.c_void_p]
    core.CFBooleanGetValue.restype = ctypes.c_bool
    core.CFRelease.argtypes = [ctypes.c_void_p]
    core.CFRelease.restype = None
    session = quartz.CGSessionCopyCurrentDictionary()
    if not session:
        raise RuntimeError("Cannot read current macOS session lock state")
    key = None
    try:
        key = core.CFStringCreateWithCString(None, b"CGSSessionScreenIsLocked", 0x08000100)
        if not key:
            raise RuntimeError("Cannot read current macOS session lock state")
        value = core.CFDictionaryGetValue(session, key)
        if not value:
            return False
        if core.CFGetTypeID(value) != core.CFBooleanGetTypeID():
            raise RuntimeError("Cannot read current macOS session lock state")
        return bool(core.CFBooleanGetValue(value))
    finally:
        if key:
            core.CFRelease(key)
        core.CFRelease(session)


def require_unlocked():
    if current_session_is_locked():
        raise RuntimeError("Mac screen is locked; unlock it before running live focus checks")


def validate_observation(settings, target, windows, actual, native, displays):
    current = next((window for window in windows if window["id"] == settings["id"]), None)
    if current is None:
        raise AssertionError(f"{settings['app_name']} {settings['title']} disappeared during focus changes")
    assert not current["is_floating"], f"{settings['title']} became floating"
    assert current["layout_position"] is not None, f"{settings['title']} lost its tile"
    assert len(actual) == 4 and actual[2] > 0 and actual[3] > 0, "Native settings frame is unavailable"
    frame = current["frame"]
    expected = [frame["origin"]["x"], frame["origin"]["y"],
                frame["size"]["width"], frame["size"]["height"]]
    assert all(abs(a - b) < 1 for a, b in zip(actual, expected)), f"{settings['title']} native frame {actual} differs from tile {expected}"
    if target["id"] == settings["id"]:
        revealed = False
        for display in displays:
            if not display["is_visible_space"]:
                continue
            viewport = display["frame"]
            x, y = viewport["origin"]["x"], viewport["origin"]["y"]
            width, height = viewport["size"]["width"], viewport["size"]["height"]
            overlap_width = min(actual[0] + actual[2], x + width) - max(actual[0], x)
            overlap_height = min(actual[1] + actual[3], y + height) - max(actual[1], y)
            revealed |= overlap_width >= min(actual[2], width) - 1 and overlap_height >= min(actual[3], height) - 1
        assert revealed, f"Focused {settings['title']} remains outside the visible viewport"
    assert len(native) == 2, "Native focused window is unavailable"
    assert int(native[0]) == target["id"]["pid"], f"Native frontmost {native} differs from {target['app_name']} {target['title']}"
    if target["bundle_id"] == settings["bundle_id"]:
        assert native[1] == target["title"], f"Native focus is {native[1]}, expected {target['title']}"
    selected = next((window for window in windows if window["is_focused"]), None)
    assert selected is not None and selected["id"] == target["id"], f"Wm focus differs from {target['title']}"


def option_l():
    require_unlocked()
    quartz = ctypes.CDLL("/System/Library/Frameworks/CoreGraphics.framework/CoreGraphics")
    core = ctypes.CDLL("/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation")
    quartz.CGEventSourceCreate.argtypes = [ctypes.c_int]
    quartz.CGEventSourceCreate.restype = ctypes.c_void_p
    quartz.CGEventCreateKeyboardEvent.argtypes = [ctypes.c_void_p, ctypes.c_uint16, ctypes.c_bool]
    quartz.CGEventCreateKeyboardEvent.restype = ctypes.c_void_p
    quartz.CGEventSetFlags.argtypes = [ctypes.c_void_p, ctypes.c_uint64]
    quartz.CGEventSetFlags.restype = None
    quartz.CGEventPost.argtypes = [ctypes.c_uint32, ctypes.c_void_p]
    quartz.CGEventPost.restype = None
    core.CFRelease.argtypes = [ctypes.c_void_p]
    core.CFRelease.restype = None
    source = quartz.CGEventSourceCreate(1)
    assert source, "Cannot create HID event source"
    try:
        for down in [True, False]:
            event = quartz.CGEventCreateKeyboardEvent(source, 37, down)
            assert event, "Cannot create Option+L event"
            try:
                quartz.CGEventSetFlags(event, (1 << 19) if down else 0)
                quartz.CGEventPost(0, event)
            finally:
                core.CFRelease(event)
            time.sleep(0.05)
    finally:
        core.CFRelease(source)


def run(args):
    require_unlocked()
    client = str(args.client.resolve())
    profile = PROFILES[args.app]
    app_name = profile["name"]
    bundle = profile["bundle"]
    settings_name = profile["settings"]

    def query(kind="windows"):
        return json.loads(subprocess.check_output([client, "query", kind], timeout=4))

    def focus(window):
        require_unlocked()
        subprocess.run(
            [client, "execute", "window", "focus", "--window-id", json.dumps(window["id"]),
             "--window-server-id", str(window["window_server_id"])],
            check=True, stdout=subprocess.DEVNULL, timeout=4,
        )

    deadline = time.monotonic() + 5
    while True:
        original = query()
        app_available = any(w["bundle_id"] == bundle for w in original)
        other_available = not args.other_bundle or any(w["bundle_id"] == args.other_bundle for w in original)
        if (app_available and other_available) or time.monotonic() >= deadline:
            break
        time.sleep(0.1)
    other = next((w for w in original if args.other_bundle and w["bundle_id"] == args.other_bundle), None)
    if args.other_bundle and other is None:
        raise AssertionError(f"No managed window for {args.other_bundle}")
    if other is None:
        other = next((w for w in original if w["bundle_id"] != bundle and w["is_focused"]), None)
    if other is None:
        other = next((w for w in original if w["bundle_id"] != bundle), None)
    if other is None:
        raise AssertionError("Another managed application is required")

    applescript(f'tell application "{app_name}" to activate')
    time.sleep(0.3)
    applescript(f'''tell application "System Events" to tell process "{app_name}"
        click menu item "{profile['menu']}" of menu 1 of menu bar item "{app_name}" of menu bar 1
    end tell''')
    time.sleep(0.4)
    windows = query()
    settings = next((w for w in windows if w["bundle_id"] == bundle and w["title"] == settings_name), None)
    main = next((w for w in windows if w["bundle_id"] == bundle and w["title"] != settings_name), None)
    if settings is None or main is None:
        raise AssertionError(f"{app_name} and {settings_name} must both be managed")

    def check(target):
        require_unlocked()
        windows = query()
        values = applescript(f'''tell application "System Events" to tell process "{app_name}"
            set p to position of window "{settings_name}"
            set s to size of window "{settings_name}"
            return {{item 1 of p, item 2 of p, item 1 of s, item 2 of s}}
        end tell''')
        actual = [float(value) for value in values.split(", ")]
        native = applescript('''tell application "System Events"
            set p to first application process whose frontmost is true
            return {unix id of p, name of value of attribute "AXFocusedWindow" of p}
        end tell''').split(", ", 1)
        validate_observation(settings, target, windows, actual, native, query("displays"))

    def settle(target):
        deadline = time.monotonic() + 5
        while True:
            try:
                check(target)
                time.sleep(0.15)
                check(target)
                return
            except (AssertionError, subprocess.CalledProcessError) as error:
                if time.monotonic() >= deadline:
                    raise AssertionError(str(error)) from error
                time.sleep(0.1)

    for cycle in range(args.cycles):
        for target in [settings, main, other, main, settings]:
            focus(target)
            settle(target)
            if args.app == "muse" and target["id"] == main["id"]:
                windows = query()
                current_main = next(w for w in windows if w["id"] == main["id"])
                current_settings = next(w for w in windows if w["id"] == settings["id"])
                assert current_settings["layout_position"]["column"] == current_main["layout_position"]["column"] + 1, "Settings must occupy the column immediately right of Muse for Option+L verification"
                option_l()
                settle(settings)
        print(f"PASS: focus cycle {cycle + 1} preserves {app_name} {settings_name}, native focus, and tiled geometry", flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description="Verify settings windows survive repeated native focus changes.")
    parser.add_argument("--app", choices=PROFILES, default="muse")
    parser.add_argument("--other-bundle", help="Bundle ID of the other managed application to focus.")
    parser.add_argument("--client", type=Path, default=Path(__file__).resolve().parents[1] / "target/debug/wm")
    parser.add_argument("--cycles", type=int, default=3)
    arguments = parser.parse_args()
    if arguments.cycles < 1:
        parser.error("--cycles must be positive")
    run(arguments)
