#!/usr/bin/env python3
# Modified by kdnk for wm on 2026-11-14. Original attribution is preserved in LICENSE.
import copy
import importlib.util
from pathlib import Path
import sys
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
source = Path(__file__).with_name("rift-settings-focus-live-test.py")
spec = importlib.util.spec_from_file_location("wm_settings_focus", source)
verifier = importlib.util.module_from_spec(spec)
spec.loader.exec_module(verifier)


class SettingsFocusVerifierTest(unittest.TestCase):
    def setUp(self):
        self.settings = {
            "id": {"pid": 10, "idx": 2}, "bundle_id": "com.meta.endo", "app_name": "Muse",
            "title": "Settings", "is_floating": False, "layout_position": {"column": 1, "row": 0},
            "is_focused": True,
            "frame": {"origin": {"x": 100, "y": 31}, "size": {"width": 840, "height": 1859}},
        }
        self.windows = [copy.deepcopy(self.settings)]
        self.actual = [100, 31, 840, 1859]
        self.native = ["10", "Settings"]
        self.displays = [{"is_visible_space": True, "frame": {
            "origin": {"x": 0, "y": 31}, "size": {"width": 3360, "height": 1859},
        }}]

    def verify(self, target=None):
        verifier.validate_observation(
            self.settings, target or self.settings, self.windows, self.actual, self.native, self.displays,
        )

    def test_visible_native_settings_focus_passes(self):
        self.verify()

    def test_app_activation_without_settings_focus_fails(self):
        self.native[1] = "Muse"
        with self.assertRaisesRegex(AssertionError, "Native focus is Muse, expected Settings"):
            self.verify()

    def test_query_focus_without_native_app_focus_fails(self):
        self.native[0] = "20"
        with self.assertRaisesRegex(AssertionError, "Native frontmost"):
            self.verify()

    def test_native_focus_without_wm_focus_fails(self):
        self.windows[0]["is_focused"] = False
        with self.assertRaisesRegex(AssertionError, "Wm focus differs"):
            self.verify()

    def test_matching_offscreen_frames_do_not_prove_visibility(self):
        for x in [3359, 100000]:
            with self.subTest(x=x):
                self.windows[0]["frame"]["origin"]["x"] = x
                self.actual[0] = x
                with self.assertRaisesRegex(AssertionError, "outside the visible viewport"):
                    self.verify()

    def test_hidden_display_does_not_prove_visibility(self):
        self.displays[0]["is_visible_space"] = False
        with self.assertRaisesRegex(AssertionError, "outside the visible viewport"):
            self.verify()

    def test_layout_membership_without_native_geometry_fails(self):
        self.actual[0] += 50
        with self.assertRaisesRegex(AssertionError, "differs from tile"):
            self.verify()

    def test_missing_window_does_not_pass(self):
        self.windows = []
        with self.assertRaisesRegex(AssertionError, "disappeared"):
            self.verify()

    def test_floating_or_untiled_settings_do_not_pass(self):
        for key, value in [("is_floating", True), ("layout_position", None)]:
            with self.subTest(key=key):
                self.windows = [copy.deepcopy(self.settings)]
                self.windows[0][key] = value
                with self.assertRaises(AssertionError):
                    self.verify()

    def test_parking_settings_while_another_app_is_focused_passes(self):
        self.windows[0]["is_focused"] = False
        self.windows[0]["frame"]["origin"]["x"] = 100000
        self.actual[0] = 100000
        other = {
            "id": {"pid": 20, "idx": 3}, "bundle_id": "com.superlogical.rex",
            "app_name": "Rex Beta", "title": "Rex", "is_focused": True,
        }
        self.windows.append(other)
        self.native = ["20", "Rex"]
        self.verify(other)

    def test_locked_screen_is_a_blocker(self):
        with patch.object(verifier, "current_session_is_locked", return_value=True):
            with self.assertRaisesRegex(RuntimeError, "Mac screen is locked"):
                verifier.require_unlocked()

    def test_native_session_lock_overrides_stale_ioreg_state(self):
        with patch.object(verifier, "current_session_is_locked", return_value=True), \
                patch.object(verifier.subprocess, "check_output", return_value='"IOConsoleLocked" = No'):
            with self.assertRaisesRegex(RuntimeError, "Mac screen is locked"):
                verifier.require_unlocked()

    def test_unlocked_screen_allows_verification(self):
        with patch.object(verifier, "current_session_is_locked", return_value=False):
            verifier.require_unlocked()

    def test_unavailable_session_prevents_verification(self):
        error = RuntimeError("Cannot read current macOS session lock state")
        with patch.object(verifier, "current_session_is_locked", side_effect=error):
            with self.assertRaisesRegex(RuntimeError, "Cannot read current macOS session lock state"):
                verifier.require_unlocked()


if __name__ == "__main__":
    unittest.main()
