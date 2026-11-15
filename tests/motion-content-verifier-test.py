#!/usr/bin/env python3
# Modified by kdnk for wm on 2026-11-15. Original attribution is preserved in LICENSE.
import copy
import importlib.util
from pathlib import Path
import unittest


SPEC = importlib.util.spec_from_file_location(
    "content_render", Path(__file__).with_name("rift-content-render-live-test.py")
)
CONTENT = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(CONTENT)
TITLE = "wm-content-fixture-0"


def sample(width, generation, painted_width=None):
    return {
        "kind": "sample", "frames": {TITLE: [0, 45, width, 822]},
        "content": {TITLE: {"generation": generation, "size": [width, 800],
                            "painted_size": [width if painted_width is None else painted_width, 800]}},
    }


def paint(width, generation):
    return {"kind": "paint_end", "title": TITLE, "native": [0, 45, width, 822],
            "generation": generation, "content_size": [width, 800]}


def resize_trace(widths=(450, 500, 550, 600)):
    initial = sample(400, 1)
    records = []
    for generation, width in enumerate(widths, 2):
        records.extend([paint(width, generation), sample(width, generation)])
    return initial, records


class ContentVerifierTests(unittest.TestCase):
    def test_accepts_painting_through_intermediate_sizes(self):
        initial, records = resize_trace()
        self.assertEqual(CONTENT.validate_content(initial, records, TITLE, 2),
                         {"paint_count": 4, "stale_samples": 0, "native_size_mismatches": 0})

    def test_accepts_small_resize_without_an_interior_pixel_size(self):
        initial, records = resize_trace((402,))
        CONTENT.validate_content(initial, records, TITLE, 2)

    def test_accepts_vertical_painting_with_distinct_native_and_content_heights(self):
        initial, records = resize_trace()
        for record in [initial, *records]:
            if record["kind"] == "sample":
                width = record["frames"][TITLE][2]
                record["frames"][TITLE][2:] = [800, width + 22]
                state = record["content"][TITLE]
                state["size"] = [800, width]
                state["painted_size"] = [800, width]
            else:
                width = record["native"][2]
                record["native"][2:] = [800, width + 22]
                record["content_size"] = [800, width]
        CONTENT.validate_content(initial, records, TITLE, 3)

    def test_accepts_unchanged_content_without_repainting(self):
        CONTENT.validate_content(sample(400, 3), [sample(400, 3)], TITLE, 2)

    def test_reports_transient_paint_lag_without_a_duration_threshold(self):
        initial, records = resize_trace()
        records.insert(0, sample(450, 1, painted_width=400))
        self.assertEqual(CONTENT.validate_content(initial, records, TITLE, 2)["stale_samples"], 1)

    def test_rejects_stale_final_painted_size(self):
        initial, records = resize_trace()
        records[-1]["content"][TITLE]["painted_size"] = [550, 800]
        with self.assertRaisesRegex(AssertionError, "settled content has stale painted size"):
            CONTENT.validate_content(initial, records, TITLE, 2)

    def test_rejects_frozen_generation_despite_updated_native_geometry(self):
        initial, records = resize_trace()
        records = [record for record in records if record["kind"] == "sample"]
        for record in records:
            record["content"][TITLE]["generation"] = 1
        with self.assertRaisesRegex(AssertionError, "paint generation did not advance"):
            CONTENT.validate_content(initial, records, TITLE, 2)

    def test_rejects_native_resize_with_frozen_logical_content(self):
        initial = sample(400, 1)
        stale = sample(400, 1)
        stale["frames"][TITLE][2] = 600
        with self.assertRaisesRegex(AssertionError, "does not follow the native resize"):
            CONTENT.validate_content(initial, [stale], TITLE, 2)

    def test_rejects_fresh_paint_at_the_wrong_native_size(self):
        initial = sample(400, 1)
        fresh = paint(500, 2)
        fresh["native"][2] = 600
        settled = sample(500, 2)
        settled["frames"][TITLE][2] = 600
        with self.assertRaisesRegex(AssertionError, "does not follow the native resize"):
            CONTENT.validate_content(initial, [fresh, settled], TITLE, 2)

    def test_rejects_interior_paints_with_stale_logical_sizes(self):
        initial, records = resize_trace()
        for record in records[:-2]:
            if record["kind"] == "sample":
                state = record["content"][TITLE]
                width = (state["size"][0] + 400) / 2
                state["size"] = [width, 800]
                state["painted_size"] = [width, 800]
            else:
                record["content_size"][0] = (record["content_size"][0] + 400) / 2
        with self.assertRaisesRegex(AssertionError, "painted only at endpoints"):
            CONTENT.validate_content(initial, records, TITLE, 2)

    def test_rejects_wrong_size_on_the_other_axis(self):
        initial, records = resize_trace()
        for record in records:
            if record["kind"] == "sample":
                record["content"][TITLE]["size"][1] = 400
                record["content"][TITLE]["painted_size"][1] = 400
            else:
                record["content_size"][1] = 400
        with self.assertRaisesRegex(AssertionError, "does not follow the native resize"):
            CONTENT.validate_content(initial, records, TITLE, 2)

    def test_rejects_a_missing_completed_paint(self):
        initial, records = resize_trace()
        records = [record for record in records if record["kind"] == "sample"]
        with self.assertRaisesRegex(AssertionError, "no completed paint"):
            CONTENT.validate_content(initial, records, TITLE, 2)

    def test_rejects_latest_paint_for_a_different_generation(self):
        initial, records = resize_trace()
        records[-1]["content"][TITLE]["generation"] = 6
        with self.assertRaisesRegex(AssertionError, "final paint does not match"):
            CONTENT.validate_content(initial, records, TITLE, 2)

    def test_rejects_painting_only_after_native_resize_finishes(self):
        initial = sample(400, 1)
        records = [sample(width, 1, painted_width=400) for width in (450, 500, 550)]
        records.extend([paint(600, 2), sample(600, 2)])
        with self.assertRaisesRegex(AssertionError, "painted only at endpoints"):
            CONTENT.validate_content(initial, records, TITLE, 2)

    def test_rejects_repainting_stale_logical_size_during_native_resize(self):
        initial = sample(400, 1)
        records = []
        for generation, width in enumerate((450, 500, 550), 2):
            stale = paint(400, generation)
            stale["native"][2] = width
            records.extend([stale, sample(width, generation, painted_width=400)])
        records.extend([paint(600, 5), sample(600, 5)])
        with self.assertRaisesRegex(AssertionError, "painted only at endpoints"):
            CONTENT.validate_content(initial, records, TITLE, 2)

    def test_requires_fresh_paint_when_reversal_returns_to_initial_size(self):
        initial = sample(400, 1)
        records = [sample(width, 1, painted_width=400) for width in (450, 500, 550, 400)]
        with self.assertRaisesRegex(AssertionError, "paint generation did not advance"):
            CONTENT.validate_content(initial, records, TITLE, 2)

    def test_another_windows_paint_cannot_satisfy_the_check(self):
        initial, records = resize_trace()
        for record in records:
            if record["kind"] == "paint_end":
                record["title"] = "wm-content-fixture-1"
        with self.assertRaisesRegex(AssertionError, "no completed paint"):
            CONTENT.validate_content(initial, records, TITLE, 2)

    def test_rejects_missing_samples_and_missing_content_observations(self):
        initial, records = resize_trace()
        with self.assertRaisesRegex(AssertionError, "no content samples"):
            CONTENT.validate_content(initial, [], TITLE, 2)
        malformed = copy.deepcopy(records)
        del malformed[-1]["content"][TITLE]
        with self.assertRaisesRegex(AssertionError, "malformed content trace"):
            CONTENT.validate_content(initial, malformed, TITLE, 2)


if __name__ == "__main__":
    unittest.main()
