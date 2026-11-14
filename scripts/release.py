#!/usr/bin/env python3
# Modified by kdnk for wm on 2026-11-14. Original attribution is preserved in LICENSE.
"""Plan, tag, and publish a verified wm release without overwriting published assets."""

import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess
from urllib.parse import quote

VERSION = r"(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)(?:-[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?"


class ReleaseError(Exception):
    pass


def run(*arguments, missing_ok=False):
    result = subprocess.run(arguments, text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    if result.returncode:
        if missing_ok and "(HTTP 404)" in result.stderr:
            return None
        raise ReleaseError(f"Command failed: {' '.join(arguments[:3])}: {result.stderr.strip()}")
    return result.stdout


def api(path, *arguments, missing_ok=False):
    output = run("gh", "api", path, *arguments, missing_ok=missing_ok)
    if output is None:
        return None
    try:
        return json.loads(output)
    except ValueError as error:
        raise ReleaseError("GitHub returned invalid JSON.") from error


def package_version(sha):
    manifest = run("git", "show", sha + ":Cargo.toml")
    section = re.search(r"(?ms)^\[package\]\s*\n(.*?)(?=^\[|\Z)", manifest)
    match = re.search(r'^version\s*=\s*"([^"]+)"', section[1], re.M) if section else None
    if not match or not re.fullmatch(VERSION, match[1]):
        raise ReleaseError("Cargo.toml must contain a valid root package version.")
    return match[1]


def tag_commit(repo, tag):
    response = api(f"repos/{repo}/git/ref/tags/{quote(tag, safe='')}", missing_ok=True)
    if response is None:
        return None
    seen = set()
    try:
        target = response["object"]
        while target["type"] == "tag":
            sha = target["sha"]
            if sha in seen or len(seen) >= 8:
                raise ReleaseError("Invalid annotated tag chain.")
            seen.add(sha)
            target = api(f"repos/{repo}/git/tags/{sha}")["object"]
        if target["type"] != "commit" or not re.fullmatch(r"[0-9a-f]{40}", target["sha"]):
            raise ReleaseError("Release tag must resolve to a commit.")
        return target["sha"]
    except (KeyError, TypeError) as error:
        raise ReleaseError("GitHub returned an invalid tag.") from error


def verify_tag(repo, tag, sha, required=False):
    actual = tag_commit(repo, tag)
    if actual is not None and actual != sha:
        raise ReleaseError(f"{tag} points to another commit; bump the version instead of moving the tag.")
    if required and actual is None:
        raise ReleaseError(f"{tag} must exist before building or publishing.")
    return actual


def find_release(repo, tag):
    pages = api(f"repos/{repo}/releases?per_page=100", "--paginate", "--slurp")
    try:
        if not isinstance(pages, list) or any(not isinstance(page, list) for page in pages):
            raise TypeError()
        matches = [release for page in pages for release in page if release["tag_name"] == tag]
        if len(matches) > 1:
            raise ReleaseError("GitHub returned duplicate releases for the tag.")
        if matches:
            release = matches[0]
            if not isinstance(release["draft"], bool) or not isinstance(release["id"], int):
                raise TypeError()
            if not isinstance(release["assets"], list):
                raise TypeError()
            return release
        return None
    except (TypeError, KeyError) as error:
        raise ReleaseError("GitHub returned an invalid release list.") from error


def has_asset(release, name):
    return any(asset.get("name") == name for asset in release["assets"])


def plan(args):
    version = package_version(args.sha)
    result = {"release": True, "sha": args.sha, "tag": "v" + version, "version": version}
    if args.event == "push" and args.before != "0" * 40 and package_version(args.before) == version:
        return {**result, "release": False, "reason": "version_unchanged"}
    existing_tag = verify_tag(args.repo, result["tag"], args.sha)
    release = find_release(args.repo, result["tag"])
    if release is not None and not release["draft"]:
        if existing_tag is None:
            raise ReleaseError("The published release has no source tag; repair it explicitly.")
        if not has_asset(release, f"wm-universal-macos-{version}.tar.gz"):
            raise ReleaseError("The published release is missing its archive; published assets will not be replaced.")
        return {**result, "release": False, "reason": "already_published"}
    return result


def create_tag(args):
    if verify_tag(args.repo, args.tag, args.sha) is None:
        api(f"repos/{args.repo}/git/refs", "--method", "POST", "--field",
            "ref=refs/tags/" + args.tag, "--field", "sha=" + args.sha)
    verify_tag(args.repo, args.tag, args.sha, required=True)
    return {"tag": args.tag, "sha": args.sha}


def publish(args):
    asset = args.asset.resolve()
    if not asset.is_file() or asset.stat().st_size == 0:
        raise ReleaseError("The verified release archive must exist and be nonempty.")
    expected_name = f"wm-universal-macos-{args.tag[1:]}.tar.gz"
    if asset.name != expected_name:
        raise ReleaseError("The release archive name must match the version tag.")
    verify_tag(args.repo, args.tag, args.sha, required=True)
    release = find_release(args.repo, args.tag)
    if release is not None and not release["draft"]:
        if not has_asset(release, asset.name):
            raise ReleaseError("The published release is missing its archive; published assets will not be replaced.")
        return {"status": "already_published", "tag": args.tag, "url": release["html_url"]}
    prerelease = "-" in args.tag.split("+", 1)[0]
    if release is None:
        release = api(f"repos/{args.repo}/releases", "--method", "POST",
                      "--raw-field", "tag_name=" + args.tag,
                      "--raw-field", "target_commitish=" + args.sha,
                      "--raw-field", "name=" + args.tag,
                      "--field", "draft=true", "--field", "generate_release_notes=true",
                      "--field", "prerelease=" + str(prerelease).lower())
        if (not isinstance(release, dict) or release.get("tag_name") != args.tag
                or release.get("draft") is not True or type(release.get("id")) is not int):
            raise ReleaseError("GitHub did not report the prepared draft release.")
    run("gh", "release", "upload", args.tag, str(asset), "--repo", args.repo, "--clobber")
    uploaded = api(f"repos/{args.repo}/releases/{release['id']}")
    expected_digest = "sha256:" + hashlib.sha256(asset.read_bytes()).hexdigest()
    matches = [item for item in uploaded["assets"] if item["name"] == asset.name]
    if not uploaded["draft"] or len(matches) != 1 or matches[0]["size"] != asset.stat().st_size:
        raise ReleaseError("The draft archive was not uploaded completely; release remains unpublished.")
    if matches[0].get("digest") not in (None, expected_digest):
        raise ReleaseError("The uploaded archive digest differs; release remains unpublished.")
    run("gh", "release", "edit", args.tag, "--repo", args.repo, "--draft=false",
        "--prerelease=" + str(prerelease).lower())
    published = api(f"repos/{args.repo}/releases/{release['id']}")
    if published["draft"] or not has_asset(published, asset.name):
        raise ReleaseError("GitHub did not confirm publication with the archive.")
    return {"status": "published", "tag": args.tag, "url": published["html_url"]}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)
    planning = subparsers.add_parser("plan")
    planning.add_argument("--event", choices=["push", "workflow_dispatch"], required=True)
    planning.add_argument("--before", default="0" * 40)
    planning.add_argument("--output", type=Path)
    tagging = subparsers.add_parser("tag")
    publishing = subparsers.add_parser("publish")
    publishing.add_argument("--asset", type=Path, required=True)
    for command in [planning, tagging, publishing]:
        command.add_argument("--repo", required=True)
        command.add_argument("--sha", required=True)
    for command in [tagging, publishing]:
        command.add_argument("--tag", required=True)
    args = parser.parse_args()
    if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", args.repo):
        raise ReleaseError("Use owner/repository for --repo.")
    if not re.fullmatch(r"[0-9a-f]{40}", args.sha):
        raise ReleaseError("The release source must be a full commit SHA.")
    if args.command == "plan":
        if not re.fullmatch(r"[0-9a-f]{40}", args.before):
            raise ReleaseError("The previous revision must be a full commit SHA.")
        result = plan(args)
        if args.output:
            with args.output.open("a") as output:
                for key in ["release", "sha", "tag", "version"]:
                    value = result[key]
                    output.write(f"{key}={str(value).lower() if isinstance(value, bool) else value}\n")
    else:
        if not args.tag.startswith("v") or not re.fullmatch(VERSION, args.tag[1:]):
            raise ReleaseError("The release tag must be v followed by a package version.")
        if args.tag != "v" + package_version(args.sha):
            raise ReleaseError("The release tag must match Cargo.toml at the source commit.")
        result = create_tag(args) if args.command == "tag" else publish(args)
    print(json.dumps(result, sort_keys=True))


if __name__ == "__main__":
    try:
        main()
    except (ReleaseError, OSError) as error:
        raise SystemExit(str(error))
