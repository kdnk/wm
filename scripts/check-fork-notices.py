#!/usr/bin/env python3
# Modified by kdnk for wm on 2026-11-14. Original attribution is preserved in LICENSE.
"""Check or add dated modification notices without replacing upstream attribution."""

import argparse
import datetime
from pathlib import Path
import re
import subprocess

ROOT = Path(__file__).resolve().parents[1]
UPSTREAM = "6101a78a4958e942853eb21cb67f8012fb293458"
PORT = "61eb68bb080c0c3c08bbb2c6c465a9b89c289329"
CGS_CHANGES = {
    "cgs/src/compositor/preview.rs",
    "cgs/src/compositor/preview/capture.rs",
    "cgs/src/compositor/preview/session.rs",
    "cgs/Cargo.toml",
}
MARKER = "Modified by kdnk for wm on "


def changed_paths():
    tracked = subprocess.check_output(["git", "diff", "--name-only", UPSTREAM], cwd=ROOT, text=True)
    added = subprocess.check_output(["git", "ls-files", "--others", "--exclude-standard"],
                                    cwd=ROOT, text=True)
    return sorted(set(tracked.splitlines() + added.splitlines()))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--write", action="store_true")
    parser.add_argument("--date", default=datetime.date.today().isoformat())
    args = parser.parse_args()
    datetime.date.fromisoformat(args.date)
    for revision, filename in [(UPSTREAM, "LICENSE"), (PORT, "cgs/LICENSE"),
                               (PORT, "cgs/third-party/ProgressiveBlurHeader.LICENSE")]:
        original = subprocess.check_output(["git", "show", f"{revision}:{filename}"], cwd=ROOT)
        if original not in (ROOT / filename).read_bytes():
            raise SystemExit("Original license text changed: " + filename)
    missing = []
    for filename in changed_paths():
        path = ROOT / filename
        if not path.is_file() or filename.startswith("cgs/") and filename not in CGS_CHANGES:
            continue
        suffix = path.suffix
        if suffix == ".rs":
            prefix = "// "
        elif suffix in {".toml", ".yml", ".yaml", ".sh", ".py"}:
            prefix = "# "
        elif filename == "assets/Info.plist":
            prefix = "<!-- "
        else:
            continue
        text = path.read_text()
        if re.search(re.escape(MARKER) + r"\d{4}-\d{2}-\d{2}", text):
            continue
        if not args.write:
            missing.append(filename)
            continue
        notice = prefix + MARKER + args.date + ". Original attribution is preserved in LICENSE."
        if prefix == "<!-- ":
            notice += " -->"
        if text.startswith("#!") and suffix in {".py", ".sh"} or text.startswith("<?xml"):
            first, remaining = text.split("\n", 1)
            path.write_text(first + "\n" + notice + "\n" + remaining)
        else:
            path.write_text(notice + "\n" + text)
    if missing:
        raise SystemExit("Missing fork modification notices: " + ", ".join(missing))
    print("PASS: modified code and build files retain dated fork notices")


if __name__ == "__main__":
    main()
