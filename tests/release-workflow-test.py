#!/usr/bin/env python3
# Modified by kdnk for wm on 2026-11-14. Original attribution is preserved in LICENSE.
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts/release.py"
SHA = "a" * 40
BEFORE = "b" * 40
OTHER = "c" * 40
TAG = "v1.2.3"
FAKE_GH = r'''#!/usr/bin/env python3
import hashlib,json,os,sys
from pathlib import Path
from urllib.parse import unquote
path=Path(os.environ['RELEASE_TEST_STATE'])
state=json.loads(path.read_text())
a=sys.argv[1:]
state['calls'].append(a)
def save(): path.write_text(json.dumps(state))
def fail(message,status=1):
    save(); print(message,file=sys.stderr); raise SystemExit(status)
def create_release(tag,prerelease):
    if any(release['tag_name']==tag for release in state['releases']): fail('Release already exists')
    assert tag in state['tags']
    release={'id':len(state['releases'])+1,'tag_name':tag,'draft':True,'assets':[],
             'html_url':'https://github.com/kdnk/wm/releases/tag/'+tag,
             'prerelease':prerelease}
    state['releases'].append(release)
    if state.pop('fail_after_create',False): fail('Connection lost after draft creation')
    return release
if state.get('network_error'): fail('gh: authentication failed (HTTP 401)')
if a[0]=='api':
    endpoint=a[1]
    if '/git/ref/tags/' in endpoint:
        tag=unquote(endpoint.split('/git/ref/tags/',1)[1])
        if tag not in state['tags']: fail('gh: Not Found (HTTP 404)')
        answer={'object':state['tags'][tag]}
    elif '/git/tags/' in endpoint:
        answer={'object':state['tag_objects'][endpoint.rsplit('/',1)[1]]}
    elif endpoint.endswith('/git/refs'):
        fields=[a[i+1] for i in range(len(a)-1) if a[i]=='--field']
        fields=dict(field.split('=',1) for field in fields)
        tag=fields['ref'].removeprefix('refs/tags/')
        if tag in state['tags']: fail('gh: Reference exists (HTTP 422)')
        state['tags'][tag]={'type':'commit','sha':fields['sha']}
        answer={'object':state['tags'][tag]}
    elif endpoint.endswith('/releases') and 'POST' in a:
        fields=[a[i+1] for i in range(len(a)-1) if a[i] in ('--field','--raw-field')]
        fields=dict(field.split('=',1) for field in fields)
        assert fields['draft']=='true' and fields['generate_release_notes']=='true'
        target=state['tags'][fields['tag_name']]
        while target['type']=='tag': target=state['tag_objects'][target['sha']]
        assert fields['target_commitish']==target['sha']
        assert fields['name']==fields['tag_name']
        answer=create_release(fields['tag_name'],fields['prerelease']=='true')
    elif '/releases?' in endpoint:
        answer=[state.get('release_list_snapshot',state['releases'])]
    elif '/releases/' in endpoint:
        identifier=int(endpoint.rsplit('/',1)[1])
        answer=next(release for release in state['releases'] if release['id']==identifier)
    else: fail('Unexpected API endpoint: '+endpoint)
elif a[:2]==['release','upload']:
    release=next(item for item in state['releases'] if item['tag_name']==a[2])
    assert release['draft'], 'A published release must never receive an upload'
    if state.pop('fail_upload',False): fail('Upload interrupted')
    asset=Path(a[3]); payload=asset.read_bytes()
    release['assets']=[item for item in release['assets'] if item['name']!=asset.name]
    release['assets'].append({'name':asset.name,'size':len(payload),
                             'digest':'sha256:'+hashlib.sha256(payload).hexdigest()})
    if state.get('bad_digest'): release['assets'][-1]['digest']='sha256:wrong'
    answer={}
elif a[:2]==['release','edit']:
    release=next(item for item in state['releases'] if item['tag_name']==a[2])
    assert release['assets'], 'Publishing without assets is forbidden'
    release['draft']=False
    release['prerelease']='--prerelease=true' in a
    answer=release
else: fail('Unexpected gh command')
save(); print(json.dumps(answer))
'''
FAKE_GIT = r'''#!/usr/bin/env python3
import json,os,sys
from pathlib import Path
state=json.loads(Path(os.environ['RELEASE_TEST_STATE']).read_text())
assert sys.argv[1]=='show', 'Only read-only git show is permitted'
revision=sys.argv[2].split(':',1)[0]
print('[package]\nname = "wm"\nversion = "'+state['versions'][revision]+'"\n\n[dependencies]\nother = "9.9.9"')
'''


def read_workflow(name):
    ruby = '''require "yaml"; require "json"; data = YAML.load_file(ARGV[0]); data["on"] = data.delete(true) if data.key?(true); puts JSON.generate(data)'''
    return json.loads(subprocess.check_output(
        ["ruby", "-e", ruby, str(ROOT / ".github/workflows" / name)], text=True))


class ReleaseTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="wm-release-fixture-")
        self.addCleanup(self.temporary.cleanup)
        self.directory = Path(self.temporary.name)
        binary = self.directory / "bin"
        binary.mkdir()
        for name, content in [("gh", FAKE_GH), ("git", FAKE_GIT)]:
            path = binary / name
            path.write_text(content)
            path.chmod(0o700)
        self.state_path = self.directory / "state.json"
        self.write_state({"calls": [], "versions": {SHA: "1.2.3", BEFORE: "1.2.2"},
                          "tags": {}, "tag_objects": {}, "releases": []})
        self.environment = dict(os.environ, RELEASE_TEST_STATE=str(self.state_path),
                                PATH=str(binary) + os.pathsep + os.environ.get("PATH", ""))
        self.asset = self.directory / "wm-universal-macos-1.2.3.tar.gz"
        self.asset.write_bytes(b"verified release archive fixture")

    def state(self):
        return json.loads(self.state_path.read_text())

    def write_state(self, state):
        self.state_path.write_text(json.dumps(state))

    def invoke(self, command, *extra, successful=True):
        result = subprocess.run([sys.executable, str(SCRIPT), command, "--repo", "kdnk/wm",
                                 "--sha", SHA, *extra], text=True, capture_output=True,
                                env=self.environment)
        if successful:
            self.assertEqual(result.returncode, 0, result.stderr)
            return json.loads(result.stdout)
        self.assertNotEqual(result.returncode, 0, result.stdout)
        return result.stderr

    def create_tag(self, tag=TAG):
        return self.invoke("tag", "--tag", tag)

    def publish(self, tag=TAG, successful=True):
        return self.invoke("publish", "--tag", tag, "--asset", str(self.asset), successful=successful)

    def test_automatic_version_change_emits_pinned_plan_and_outputs(self):
        output = self.directory / "output"
        plan = self.invoke("plan", "--event", "push", "--before", BEFORE, "--output", str(output))
        self.assertEqual(plan, {"release": True, "sha": SHA, "tag": TAG, "version": "1.2.3"})
        self.assertIn("release=true\n", output.read_text())
        self.assertIn("sha=" + SHA + "\n", output.read_text())
        self.assertFalse(self.state()["tags"])

    def test_unchanged_version_skips_without_remote_access(self):
        state = self.state()
        state["versions"][BEFORE] = "1.2.3"
        state["network_error"] = True
        self.write_state(state)
        result = self.invoke("plan", "--event", "push", "--before", BEFORE)
        self.assertEqual(result["reason"], "version_unchanged")
        self.assertFalse(self.state()["calls"])

    def test_manual_initial_release_does_not_require_an_existing_release(self):
        self.assertTrue(self.invoke("plan", "--event", "workflow_dispatch")["release"])
        self.create_tag()
        self.assertEqual(self.publish()["status"], "published")
        state = self.state()
        self.assertEqual(state["tags"][TAG]["sha"], SHA)
        self.assertFalse(state["releases"][0]["draft"])
        self.assertEqual([asset["name"] for asset in state["releases"][0]["assets"]], [self.asset.name])

    def test_conflicting_tag_never_moves_or_publishes(self):
        state = self.state()
        state["tags"][TAG] = {"type": "commit", "sha": OTHER}
        self.write_state(state)
        self.assertIn("another commit", self.invoke("plan", "--event", "workflow_dispatch", successful=False))
        self.assertIn("another commit", self.invoke("tag", "--tag", TAG, successful=False))
        self.assertEqual(self.state()["tags"][TAG]["sha"], OTHER)
        self.assertFalse(self.state()["releases"])

    def test_stale_release_list_does_not_hide_the_created_draft(self):
        self.create_tag()
        state = self.state()
        state["release_list_snapshot"] = []
        self.write_state(state)
        self.assertEqual(self.publish()["status"], "published")
        release, = self.state()["releases"]
        self.assertFalse(release["draft"])
        self.assertEqual(release["assets"], [{"name": self.asset.name,
                                           "size": self.asset.stat().st_size,
                                           "digest": "sha256:" + hashlib.sha256(self.asset.read_bytes()).hexdigest()}])

    def test_annotated_tag_resolves_to_the_source_commit(self):
        state = self.state()
        state["tags"][TAG] = {"type": "tag", "sha": OTHER}
        state["tag_objects"][OTHER] = {"type": "commit", "sha": SHA}
        self.write_state(state)
        self.create_tag()
        self.assertFalse(any("POST" in call for call in self.state()["calls"]))
        self.publish()

    def test_network_failures_do_not_become_missing_releases(self):
        state = self.state()
        state["network_error"] = True
        self.write_state(state)
        self.assertIn("Command failed", self.invoke("plan", "--event", "workflow_dispatch", successful=False))
        self.assertFalse(self.state()["tags"])
        self.assertFalse(self.state()["releases"])

    def test_tag_retry_does_not_recreate_the_ref(self):
        self.create_tag()
        self.create_tag()
        self.assertEqual(sum("POST" in call for call in self.state()["calls"]), 1)

    def test_upload_failure_leaves_a_draft_then_retry_completes_it(self):
        self.create_tag()
        state = self.state()
        state["fail_upload"] = True
        self.write_state(state)
        self.assertIn("Command failed", self.publish(successful=False))
        self.assertTrue(self.state()["releases"][0]["draft"])
        self.assertEqual(self.publish()["status"], "published")
        self.assertEqual(len(self.state()["releases"]), 1)

    def test_lost_draft_creation_response_is_recoverable(self):
        self.create_tag()
        state = self.state()
        state["fail_after_create"] = True
        self.write_state(state)
        self.publish(successful=False)
        self.assertTrue(self.state()["releases"][0]["draft"])
        self.publish()
        self.assertEqual(len(self.state()["releases"]), 1)

    def test_published_release_is_never_overwritten(self):
        self.create_tag()
        self.publish()
        before = self.state()
        self.assertEqual(self.publish()["status"], "already_published")
        self.assertEqual(self.invoke("plan", "--event", "workflow_dispatch")["reason"], "already_published")
        self.assertEqual(self.state()["releases"], before["releases"])
        self.assertEqual([call for call in self.state()["calls"] if call[0] == "release"],
                         [call for call in before["calls"] if call[0] == "release"])

    def test_missing_published_asset_requires_review_instead_of_overwrite(self):
        self.create_tag()
        self.publish()
        state = self.state()
        state["releases"][0]["assets"] = []
        self.write_state(state)
        self.assertIn("missing its archive", self.publish(successful=False))
        self.assertIn("missing its archive", self.invoke("plan", "--event", "workflow_dispatch", successful=False))

    def test_published_release_without_a_source_tag_does_not_skip_verification(self):
        self.create_tag()
        self.publish()
        state = self.state()
        state["tags"].clear()
        self.write_state(state)
        self.assertIn("no source tag", self.invoke("plan", "--event", "workflow_dispatch", successful=False))

    def test_missing_or_wrong_archive_cannot_publish(self):
        self.create_tag()
        self.asset.unlink()
        self.assertIn("nonempty", self.publish(successful=False))
        self.asset = self.directory / "wm-universal-macos-9.9.9.tar.gz"
        self.asset.write_bytes(b"wrong archive")
        self.assertIn("match the version", self.publish(successful=False))
        self.assertFalse(self.state()["releases"])

    def test_corrupt_uploaded_archive_stays_unpublished(self):
        self.create_tag()
        state = self.state()
        state["bad_digest"] = True
        self.write_state(state)
        self.assertIn("digest differs", self.publish(successful=False))
        self.assertTrue(self.state()["releases"][0]["draft"])

    def test_prerelease_version_keeps_prerelease_status(self):
        state = self.state()
        state["versions"][SHA] = "1.2.3-rc.1"
        self.write_state(state)
        tag = self.invoke("plan", "--event", "workflow_dispatch")["tag"]
        self.asset = self.directory / "wm-universal-macos-1.2.3-rc.1.tar.gz"
        self.asset.write_bytes(b"prerelease fixture")
        self.create_tag(tag)
        self.publish(tag)
        self.assertTrue(self.state()["releases"][0]["prerelease"])

    def test_input_validation_rejects_shell_and_ref_injection(self):
        self.assertIn("release tag", self.invoke("tag", "--tag", "v1.2.3;echo unsafe", successful=False))
        state = self.state()
        state["versions"][SHA] = '1.2.3$(echo unsafe)'
        self.write_state(state)
        self.assertIn("valid root package version", self.invoke("plan", "--event", "workflow_dispatch", successful=False))
        self.assertFalse(self.state()["calls"])

    def test_valid_tag_for_another_package_version_is_rejected_before_remote_writes(self):
        self.assertIn("match Cargo.toml", self.invoke("tag", "--tag", "v9.9.9", successful=False))
        self.assertIn("match Cargo.toml", self.publish("v9.9.9", successful=False))
        self.assertFalse(self.state()["calls"])

    def test_workflow_connects_verified_source_to_tag_build_and_publication(self):
        workflow = read_workflow("release.yml")
        triggers = workflow["on"]
        self.assertNotIn("release", triggers)
        self.assertEqual(triggers["push"]["branches"], ["main"])
        self.assertEqual(triggers["push"]["paths"], ["Cargo.toml"])
        self.assertIn("workflow_dispatch", triggers)
        jobs = workflow["jobs"]
        self.assertEqual(jobs["verify"]["uses"], "./.github/workflows/rust.yml")
        self.assertEqual(jobs["verify"]["with"]["source_sha"], "${{ needs.prepare.outputs.sha }}")
        verification = read_workflow("rust.yml")
        self.assertEqual(verification["on"]["workflow_call"]["inputs"]["source_sha"]["type"], "string")
        self.assertIn("${{ inputs.source_sha || github.ref }}", verification["concurrency"]["group"])
        for job in verification["jobs"].values():
            self.assertEqual(job["steps"][0]["with"]["ref"], "${{ inputs.source_sha || github.sha }}")
        self.assertEqual(jobs["tag"]["needs"], ["prepare", "verify"])
        self.assertEqual(jobs["build"]["needs"], ["prepare", "tag"])
        self.assertEqual(jobs["publish"]["needs"], ["prepare", "build"])
        for name in ["build", "publish"]:
            checkout = jobs[name]["steps"][0]["with"]
            self.assertEqual(checkout["ref"], "${{ needs.prepare.outputs.sha }}")
            self.assertEqual(checkout["fetch-depth"], 0)
        steps = jobs["publish"]["steps"]
        names = [step.get("name") for step in steps]
        self.assertLess(names.index("Sign release binary"), names.index("Create or complete GitHub release"))
        self.assertLess(names.index("Verify packaged binary version"), names.index("Create or complete GitHub release"))
        self.assertEqual(jobs["tag"]["permissions"], {"contents": "write"})
        self.assertEqual(jobs["publish"]["permissions"], {"contents": "write"})
        self.assertEqual(workflow["permissions"], {"contents": "read"})
        self.assertFalse((ROOT / ".github/workflows/tag.yml").exists())

    def test_workflow_builds_and_packages_only_the_wm_binary(self):
        jobs = read_workflow("release.yml")["jobs"]
        build = {step.get("name"): step for step in jobs["build"]["steps"]}
        publish = {step.get("name"): step for step in jobs["publish"]["steps"]}
        tools = self.directory / "bin"
        commands = {
            "cargo": '''import sys
assert sys.argv[1:] == ['build','--release','--locked','--package','wm','--bin','wm','--target',__import__('os').environ['TEST_TARGET']]
''',
            "lipo": '''import sys
from pathlib import Path
args=sys.argv[1:]
if args[0] == '-create':
    assert len(args) == 5 and args[3] == '-output'
    assert all(Path(name).name == 'wm' and Path(name).is_file() for name in args[1:3])
    Path(args[4]).write_text('#!/bin/sh\\nprintf "wm 1.2.3\\\\n"\\n')
else:
    assert args[0] == 'dist/wm' and args[1] == '-verify_arch' and args[2] in ('arm64','x86_64')
''',
            "strip": '''import sys
assert sys.argv[1:] == ['-x','dist/wm']
''',
        }
        for name, content in commands.items():
            path = tools / name
            path.write_text(f"#!{sys.executable}\n" + content)
            path.chmod(0o700)

        def run_step(step, **extra):
            script = step["run"].replace("${{ matrix.target }}", extra.get("TEST_TARGET", ""))
            result = subprocess.run(["/bin/bash", "-c", script], cwd=self.directory,
                                    env=dict(self.environment, **extra), text=True, capture_output=True)
            self.assertEqual(result.returncode, 0, result.stderr)

        for target in jobs["build"]["strategy"]["matrix"]["target"]:
            binaries = self.directory / "target" / target / "release"
            binaries.mkdir(parents=True)
            (binaries / "wm").write_bytes(b"wm slice")
            (binaries / "wm-cli").write_bytes(b"stale client must not ship")
            run_step(build["Build release"], TEST_TARGET=target)
            run_step(build["Bundle architecture binaries"], TEST_TARGET=target)
            with tarfile.open(self.directory / "artifacts" / f"{target}.tar.gz") as archive:
                self.assertEqual(archive.getnames(), ["wm"])

        run_step(publish["Create universal binary"])
        for name in ("wm.default.toml", "assets/release-signing.pem", "LICENSE", "NOTICE",
                     "cgs/LICENSE", "cgs/third-party/ProgressiveBlurHeader.LICENSE"):
            destination = self.directory / name
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(ROOT / name, destination)
        (self.directory / "dist/wm-cli").write_bytes(b"stale client must not ship")
        run_step(publish["Package tarball"], RELEASE_TAG=TAG, ASSET_BASENAME="wm-universal-macos",
                 GITHUB_OUTPUT=str(self.directory / "outputs"))
        with tarfile.open(self.asset) as archive:
            self.assertEqual({member.name for member in archive if member.isfile()}, {
                "wm", "wm.default.toml", "release-signing.pem", "licenses/wm-LICENSE",
                "licenses/wm-NOTICE", "licenses/cgs/LICENSE", "licenses/cgs/ProgressiveBlurHeader.LICENSE",
            })
        run_step(publish["Verify packaged binary version"], VERSION="1.2.3")
        signature = publish["Sign release binary"]["run"]
        self.assertIn("--identifier git.kdnk.wm", signature)
        self.assertNotIn("wm-cli", signature)


if __name__ == "__main__":
    unittest.main()
