#!/usr/bin/env python3
# Modified by kdnk for wm on 2026-11-15. Original attribution is preserved in LICENSE.
"""Record the isolated native menu fixture without controlling the wm daemon."""

import argparse
import json
from pathlib import Path
import subprocess
import time


def record(binary, output):
    output.mkdir(parents=True, exist_ok=False)
    with (output / "fixture.log").open("w") as log:
        fixture = subprocess.Popen(
            [str(binary.resolve()), "--motion-demo", str(output.resolve())],
            stdout=log, stderr=subprocess.STDOUT,
        )
        recorder = None
        try:
            deadline = time.monotonic() + 10
            capture = output / "capture.json"
            while not capture.exists():
                if fixture.poll() is not None:
                    raise AssertionError("Fixture exited before publishing its capture region")
                assert time.monotonic() < deadline, "Fixture did not publish its capture region"
                time.sleep(0.01)
            metadata = json.loads(capture.read_text())
            assert metadata["pid"] == fixture.pid, "Capture region belongs to another process"
            assert metadata["title"] == "wm Menu Interaction", "Unexpected fixture window"
            region = ",".join(str(round(value)) for value in metadata["rect"])
            recorder = subprocess.Popen(
                ["/usr/sbin/screencapture", "-x", "-v", "-V", "4", "-R" + region,
                 str(output / "menu-motion.mov")],
                stdout=subprocess.DEVNULL, stderr=subprocess.PIPE,
            )
            _, error = recorder.communicate(timeout=10)
            assert recorder.returncode == 0, error.decode()
            assert fixture.poll() is None, "Fixture closed before native capture completed"
            subprocess.run(
                ["/usr/sbin/screencapture", "-x", "-R" + region,
                 str(output / "capture-after.png")], check=True,
            )
            assert fixture.wait(timeout=15) == 0, "Native fixture failed"
            samples = json.loads((output / "menu-motion.json").read_text())
            actions = [sample["action"] for sample in samples if sample["action"]]
            assert actions == ["focus"] * 5 + ["workspace"] * 4 + ["focus", "stop", "focus"]
            assert all(a["time"] < b["time"] for a, b in zip(samples, samples[1:])), (
                "Fixture samples must use one advancing clock")
            for sample in [next(sample for sample in samples if sample["action"] == "stop"),
                           samples[-1]]:
                for frame in sample["frames"]:
                    if frame["focus"] is not None:
                        assert frame["focus"] == frame["target"], (
                            "Drag start and completion must settle focus at its exact target")
            assert (output / "menu-motion.mov").stat().st_size > 0, "Empty native recording"
            print("PASS: native focus/workspace reversals, exact drag-start settlement, and video")
            print(output)
        finally:
            if fixture.poll() is None:
                fixture.terminate()
                fixture.wait(timeout=5)
            if recorder is not None and recorder.poll() is None:
                recorder.terminate()
                recorder.communicate(timeout=5)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--example", type=Path,
                        default=Path("target/debug/examples/menu-bar-window-list-check"))
    parser.add_argument("--output", type=Path, required=True)
    arguments = parser.parse_args()
    record(arguments.example, arguments.output)
