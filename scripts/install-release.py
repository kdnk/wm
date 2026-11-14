#!/usr/bin/env python3
# Modified by kdnk for wm on 2026-11-14. Original attribution is preserved in LICENSE.
"""Install a signed wm release without building or operating the service."""

import argparse
import hashlib
import os
from pathlib import Path
import re
import shutil
import ssl
import subprocess
import sys
import tarfile
import tempfile
from urllib.parse import unquote

RELEASES_URL = "https://github.com/kdnk/wm/releases"
PINNED_CERTIFICATE_SHA256 = "a5dcf5cd43ec7c6569673d71e3d91ed7f78b38766cdd3b1297cce9940cc9b9fc"
VERSION_TAG = r"v(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)(?:-[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?"
CURL = ("curl", "--fail", "--location", "--silent", "--show-error",
        "--proto", "=https", "--proto-redir", "=https")
FILES = {
    "wm", "wm-cli", "wm.default.toml", "release-signing.pem",
    "licenses/wm-LICENSE", "licenses/wm-NOTICE", "licenses/cgs/LICENSE",
    "licenses/cgs/ProgressiveBlurHeader.LICENSE",
}
DIRECTORIES = {"licenses", "licenses/cgs"}


class InstallError(Exception):
    pass


def run(*arguments):
    result = subprocess.run(arguments, text=True, capture_output=True)
    if result.returncode:
        raise InstallError(f"{arguments[0]} failed: {result.stderr.strip()}")
    return result.stdout


def certificate_der(path):
    try:
        return ssl.PEM_cert_to_DER_cert(path.read_text())
    except (OSError, ValueError) as error:
        raise InstallError(f"Invalid signing certificate: {path}") from error


def download_release(tag, directory):
    if shutil.which("curl") is None:
        raise InstallError("curl is required to download a release; a local archive can be installed offline.")
    if tag is None:
        latest = run(*CURL, "--head", "--output", os.devnull, "--write-out", "%{url_effective}",
                     RELEASES_URL + "/latest").strip()
        prefix = RELEASES_URL + "/tag/"
        if not latest.startswith(prefix):
            raise InstallError("GitHub did not redirect to a versioned release.")
        tag = unquote(latest[len(prefix):])
    if not re.fullmatch(VERSION_TAG, tag):
        raise InstallError("Release version must be a tag such as v0.8.2.")
    name = f"wm-universal-macos-{tag[1:]}.tar.gz"
    archive = directory / name
    run(*CURL, "--output", str(archive), f"{RELEASES_URL}/download/{tag}/{name}")
    return archive


def extract_archive(archive, destination):
    with tarfile.open(archive, "r:gz") as source:
        members = source.getmembers()
        seen = set()
        for member in members:
            name = member.name.rstrip("/")
            if name in seen:
                raise InstallError(f"Duplicate archive entry: {member.name}")
            seen.add(name)
            if member.isdir() and name in DIRECTORIES:
                continue
            if not member.isfile() or name not in FILES or member.size <= 0:
                raise InstallError(f"Unexpected archive entry: {member.name}")
            if member.size > 512 * 1024 * 1024:
                raise InstallError(f"Archive entry is too large: {member.name}")
        if not FILES.issubset(seen):
            raise InstallError("Release archive is missing required files")
        for member in members:
            if member.isdir():
                continue
            target = destination / member.name
            target.parent.mkdir(parents=True, exist_ok=True)
            with source.extractfile(member) as incoming, target.open("wb") as output:
                shutil.copyfileobj(incoming, output)
            target.chmod(0o755 if member.name in {"wm", "wm-cli"} else 0o644)


def verify_release(directory):
    pinned = certificate_der(directory / "release-signing.pem")
    if hashlib.sha256(pinned).hexdigest() != PINNED_CERTIFICATE_SHA256:
        raise InstallError("Release certificate differs from the pinned signing certificate")
    fingerprint = hashlib.sha1(pinned).hexdigest()
    for name, identifier in (("wm", "git.kdnk.wm"), ("wm-cli", "git.kdnk.wm.cli")):
        requirement = f'=identifier "{identifier}" and certificate leaf = H"{fingerprint}"'
        run("codesign", "--verify", "--strict", "--all-architectures",
            "--test-requirement", requirement, str(directory / name))


def check_directory(path):
    for parent in (*reversed(path.parents), path):
        if parent.is_symlink() or (parent.exists() and not parent.is_dir()):
            raise InstallError(f"Installation directory must not contain symlinks or files: {parent}")


def install(directory, prefix):
    targets = [prefix / "bin/wm", prefix / "bin/wm-cli", prefix / "share/wm/release"]
    for target in targets:
        check_directory(target.parent)
        if target.is_symlink():
            raise InstallError(f"Refusing to replace a symlink: {target}")
        if target.exists() and target.is_dir() != (target.name == "release"):
            raise InstallError(f"Unexpected installation target: {target}")
    prefix.mkdir(parents=True, exist_ok=True)
    stage = Path(tempfile.mkdtemp(prefix=".wm-install-", dir=prefix))
    keep_backup = False
    try:
        metadata = stage / "release"
        metadata.mkdir()
        for name in FILES:
            target = stage / name if name in {"wm", "wm-cli"} else metadata / name
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(directory / name, target)
        replacements = [(stage / "wm", targets[0]), (stage / "wm-cli", targets[1]),
                        (metadata, targets[2])]
        changed = []
        try:
            for index, (source, target) in enumerate(replacements):
                target.parent.mkdir(parents=True, exist_ok=True)
                backup = stage / f"backup-{index}"
                if target.exists():
                    os.replace(target, backup)
                changed.append((target, backup))
                os.replace(source, target)
        except OSError as error:
            try:
                for target, backup in reversed(changed):
                    if target.is_dir():
                        shutil.rmtree(target)
                    elif target.exists():
                        target.unlink()
                    if backup.exists():
                        os.replace(backup, target)
            except OSError as rollback_error:
                keep_backup = True
                raise InstallError(f"Installation and rollback failed; backups remain in {stage}: {rollback_error}") from error
            raise
    finally:
        if not keep_backup:
            shutil.rmtree(stage)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("archive", nargs="?", type=Path, help="install a local release archive")
    parser.add_argument("--version", help="download a specific release tag, such as v0.8.2")
    parser.add_argument("--prefix", type=Path, default=Path.home() / ".local")
    args = parser.parse_args()
    if args.archive and args.version:
        parser.error("archive and --version cannot be used together")
    if args.version and not re.fullmatch(VERSION_TAG, args.version):
        parser.error("--version must be a release tag such as v0.8.2")
    prefix = Path(os.path.abspath(args.prefix.expanduser()))
    with tempfile.TemporaryDirectory(prefix="wm-release-") as temporary:
        scratch = Path(temporary)
        archive = args.archive
        if archive is None:
            archive = download_release(args.version, scratch)
        extracted = scratch / "extracted"
        extracted.mkdir()
        extract_archive(archive, extracted)
        verify_release(extracted)
        install(extracted, prefix)
    print(f"Installed wm and wm-cli in {prefix / 'bin'}")
    print(f"Default configuration and licenses: {prefix / 'share/wm/release'}")
    print("Add the bin directory to PATH and configure ~/.config/wm/config.toml.")
    print("For the first start, run wm service install, then wm service start.")
    print("Stop other window managers before starting wm.")


if __name__ == "__main__":
    try:
        main()
    except (InstallError, OSError, tarfile.TarError) as error:
        sys.exit(f"install-release: {error}")
