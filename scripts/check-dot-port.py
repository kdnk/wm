#!/usr/bin/env python3
# Modified by kdnk for wm on 2026-11-14. Original attribution is preserved in LICENSE.
"""Reconstruct the dot port from recorded local source revisions."""

import argparse
from pathlib import Path
import re
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]
DOT_REVISION = "f502e0bdaac397f7ede56e2b4b384c018d8a4126"
RIFT_REVISION = "6101a78a4958e942853eb21cb67f8012fb293458"
CGS_REVISION = "bd7cdbc171904e7d6057cac074afbd72ce5c1bf9"
PORTED_REVISION = "61eb68bb080c0c3c08bbb2c6c465a9b89c289329"
PATCHES = [
    "external-focus-warp", "screen-zoom-focus", "patched-identity",
    "space-refresh-animation", "menubar-window-list", "menubar-placement",
    "closed-window-projection", "managed-attached-window", "managed-nonmodal-dialog",
    "config-validation", "scrolling-row-resize", "stacking-active-columns",
    "axis-constraints", "macos-window-motion", "live-resize-content",
]


def git(checkout, *args):
    return subprocess.check_output(["git", "-C", str(checkout), *args])


def archive(checkout, revision, destination):
    destination.mkdir()
    subprocess.run(["tar", "-x", "-C", str(destination)],
                   input=git(checkout, "archive", revision), check=True)


def apply(destination, patch):
    result = subprocess.run(["patch", "--batch", "--forward", "-p1", "-d", str(destination)],
                            input=patch, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    if result.returncode:
        raise RuntimeError(result.stdout.decode())


def source_patch(dot, name):
    return git(dot, "show", f"{DOT_REVISION}:tools/rift/{name}.patch")


def rift_patch(dot, name):
    patch = source_patch(dot, name)
    if name == "macos-window-motion":
        old = b"@@ -1418,6 +1439,8 @@\n"
        new = b"@@ -1418,7 +1439,8 @@\n"
        if patch.count(old) != 1:
            raise AssertionError("Expected animation merge hunk changed")
        patch = patch.replace(old, new)
        old = b"                 window.from = old.frame;\n+                window.frame = old.frame;\n"
        new = b"                 window.from = old.frame;\n                 window.frame = old.frame;\n"
        if patch.count(old) != 1:
            raise AssertionError("Expected inherited frame assignment changed")
        patch = patch.replace(old, new)
    return patch


def compare(expected, actual, paths):
    mismatches = [str(path) for path in paths
                  if not (actual / path).is_file()
                  or (expected / path).read_bytes() != (actual / path).read_bytes()]
    if mismatches:
        raise AssertionError("Imported sources differ: " + ", ".join(sorted(mismatches)))
    return len(paths)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("dot", type=Path)
    parser.add_argument("cgs", type=Path)
    parser.add_argument("--rustfmt", default="rustfmt", help="Path to nightly rustfmt.")
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="rift-port-check-") as directory:
        scratch = Path(directory).resolve()
        rift = scratch / "rift"
        cgs = scratch / "cgs"
        ported = scratch / "ported"
        archive(ROOT, RIFT_REVISION, rift)
        archive(ROOT, PORTED_REVISION, ported)
        paths = set()
        for name in PATCHES:
            patch = rift_patch(args.dot, name)
            apply(rift, patch)
            paths.update(Path(path) for path in re.findall(r"^\+\+\+ b/(.+)$", patch.decode(), re.M)
                         if path.endswith((".rs", ".plist")))
        apply(rift, (ROOT / "scripts/dot-port-upstream.patch").read_bytes())
        (rift / "rustfmt.toml").write_bytes((ROOT / "rustfmt.toml").read_bytes())
        subprocess.run([args.rustfmt, "--edition", "2024", "--config-path", str(rift / "rustfmt.toml"),
                        "--config", "skip_children=true",
                        *[str(rift / path) for path in sorted(paths) if path.suffix == ".rs"]],
                       check=True)
        rift_count = compare(rift, ported, paths)
        archive(args.cgs, CGS_REVISION, cgs)
        apply(cgs, source_patch(args.dot, "cgs-exact-capture"))
        cgs_paths = {path.relative_to(cgs) for path in (cgs / "src").rglob("*") if path.is_file()}
        cgs_paths.update([Path("build.rs"), Path("LICENSE"), Path("Cargo.lock"),
                          Path("third-party/ProgressiveBlurHeader.LICENSE")])
        cgs_count = compare(cgs, ported / "cgs", cgs_paths)
        print(f"PASS: archived port {PORTED_REVISION[:7]} matches {rift_count} Rift files and {cgs_count} cgs files")


if __name__ == "__main__":
    main()
