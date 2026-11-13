#!/usr/bin/env python3
# Modified by kdnk for wm on 2026-11-14. Original attribution is preserved in LICENSE.

import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import shutil
import ssl
import subprocess
import sys
import tarfile
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts/install-release.py"
spec = importlib.util.spec_from_file_location("install_release", SCRIPT)
installer = importlib.util.module_from_spec(spec)
spec.loader.exec_module(installer)

FAKE_TOOL = r'''import json,os,shutil,sys
from pathlib import Path
name=Path(sys.argv[0]).name
args=sys.argv[1:]
log=Path(os.environ['INSTALL_TEST_LOG'])
with log.open('a') as output:
    output.write(json.dumps([name,*args])+'\n')
if name == 'gh':
    assert args[:2] == ['release','download']
    assert args[args.index('--repo')+1] == 'kdnk/wm'
    assert args[args.index('--pattern')+1] == 'wm-universal-macos-*.tar.gz'
    destination=Path(args[args.index('--dir')+1])/'wm-universal-macos-0.8.2.tar.gz'
    shutil.copyfile(os.environ['INSTALL_TEST_ARCHIVE'],destination)
elif name == 'codesign':
    assert args[:4] == ['--verify','--strict','--all-architectures','--test-requirement']
    identifier='git.kdnk.wm.cli' if Path(args[-1]).name == 'wm-cli' else 'git.kdnk.wm'
    assert args[4] == '=identifier "'+identifier+'" and certificate leaf = H"'+os.environ['INSTALL_TEST_PIN']+'"'
    if os.environ.get('INSTALL_TEST_BAD_SIGNATURE') == Path(args[-1]).name:
        print('invalid signature',file=sys.stderr)
        sys.exit(1)
else:
    raise AssertionError(name)
'''


class ReleaseInstallerTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="wm-install-test-")
        self.addCleanup(self.temporary.cleanup)
        self.directory = Path(self.temporary.name).resolve()
        self.prefix = self.directory / "prefix"
        self.archive = self.directory / "release.tar.gz"
        self.log = self.directory / "calls.jsonl"
        self.home = self.directory / "home"
        self.home.mkdir()
        self.payload = {
            "wm": b"signed-daemon", "wm-cli": b"signed-client", "wm.default.toml": b"[settings]\n",
            "release-signing.pem": (ROOT / "assets/release-signing.pem").read_bytes(),
            "licenses/wm-LICENSE": b"wm license", "licenses/wm-NOTICE": b"wm notice",
            "licenses/cgs/LICENSE": b"cgs license",
            "licenses/cgs/ProgressiveBlurHeader.LICENSE": b"blur license",
        }
        self.make_archive()
        tools = self.directory / "tools"
        tools.mkdir()
        for name in ("gh", "codesign"):
            path = tools / name
            path.write_text(f"#!{sys.executable}\n" + FAKE_TOOL)
            path.chmod(0o755)
        fingerprint = hashlib.sha1(ssl.PEM_cert_to_DER_cert(
            self.payload["release-signing.pem"].decode())).hexdigest()
        self.environment = dict(os.environ, HOME=str(self.home),
                                PATH=str(tools) + os.pathsep + os.environ.get("PATH", ""),
                                INSTALL_TEST_LOG=str(self.log), INSTALL_TEST_ARCHIVE=str(self.archive),
                                INSTALL_TEST_PIN=fingerprint)

    def make_archive(self, extra=()):
        with tarfile.open(self.archive, "w:gz") as archive:
            for name, data in self.payload.items():
                member = tarfile.TarInfo(name)
                member.size = len(data)
                archive.addfile(member, io.BytesIO(data))
            for member, data in extra:
                archive.addfile(member, io.BytesIO(data) if data is not None else None)

    def invoke(self, *arguments, successful=True, environment=None):
        result = subprocess.run([sys.executable, str(SCRIPT), "--prefix", str(self.prefix), *arguments],
                                env=environment or self.environment, text=True, capture_output=True)
        self.assertEqual(result.returncode == 0, successful, result.stderr or result.stdout)
        return result

    def calls(self):
        return [json.loads(line) for line in self.log.read_text().splitlines()] if self.log.exists() else []

    def seed_installation(self):
        (self.prefix / "bin").mkdir(parents=True)
        (self.prefix / "bin/wm").write_bytes(b"old-daemon")
        (self.prefix / "bin/wm-cli").write_bytes(b"old-client")
        (self.prefix / "share/wm/release").mkdir(parents=True)
        (self.prefix / "share/wm/release/old").write_bytes(b"old-metadata")

    def assert_old_installation(self):
        self.assertEqual((self.prefix / "bin/wm").read_bytes(), b"old-daemon")
        self.assertEqual((self.prefix / "bin/wm-cli").read_bytes(), b"old-client")
        self.assertEqual((self.prefix / "share/wm/release/old").read_bytes(), b"old-metadata")

    def test_installs_verified_pair_without_touching_configuration_identity_or_service(self):
        configuration = self.home / ".config/wm/config.toml"
        configuration.parent.mkdir(parents=True)
        configuration.write_text("personal settings")
        identity = self.prefix / "share/wm/release-signing/password"
        identity.parent.mkdir(parents=True)
        identity.write_text("protected signing material")
        self.invoke(str(self.archive))
        for name, data in self.payload.items():
            target = self.prefix / "bin" / name if name in {"wm", "wm-cli"} else self.prefix / "share/wm/release" / name
            self.assertEqual(target.read_bytes(), data)
        self.assertTrue(os.access(self.prefix / "bin/wm", os.X_OK))
        self.assertEqual(configuration.read_text(), "personal settings")
        self.assertEqual(identity.read_text(), "protected signing material")
        self.assertEqual([call[0] for call in self.calls()], ["codesign", "codesign"])

    def test_downloads_latest_or_selected_release_through_github_cli(self):
        self.invoke()
        first = self.calls()[0]
        self.assertEqual(first[:3], ["gh", "release", "download"])
        self.assertIn("--repo", first)
        self.invoke("--version", "v0.8.2")
        downloads = [call for call in self.calls() if call[0] == "gh"]
        self.assertEqual(downloads[1][:4], ["gh", "release", "download", "v0.8.2"])

    def test_bad_client_signature_preserves_installed_pair(self):
        self.seed_installation()
        result = self.invoke(str(self.archive), successful=False,
                             environment=dict(self.environment, INSTALL_TEST_BAD_SIGNATURE="wm-cli"))
        self.assertIn("invalid signature", result.stderr)
        self.assert_old_installation()

    def test_unpinned_certificate_is_rejected_before_signature_verification(self):
        self.seed_installation()
        self.payload["release-signing.pem"] = b"-----BEGIN CERTIFICATE-----\nYW5vdGhlcg==\n-----END CERTIFICATE-----\n"
        self.make_archive()
        self.invoke(str(self.archive), successful=False)
        self.assertEqual(self.calls(), [])
        self.assert_old_installation()

    def test_unsafe_or_incomplete_archive_preserves_installed_pair(self):
        self.seed_installation()
        cases = []
        for name in ("../escape", "/absolute", "./wm", "unexpected", "wm"):
            member = tarfile.TarInfo(name)
            member.size = 4
            cases.append((member, b"evil"))
        for kind in (tarfile.SYMTYPE, tarfile.LNKTYPE):
            member = tarfile.TarInfo("licenses/linked")
            member.type = kind
            member.linkname = "../../outside"
            cases.append((member, None))
        for entry in cases:
            with self.subTest(name=entry[0].name, kind=entry[0].type):
                self.make_archive([entry])
                self.invoke(str(self.archive), successful=False)
                self.assert_old_installation()
        del self.payload["wm-cli"]
        self.make_archive()
        self.invoke(str(self.archive), successful=False)
        self.assert_old_installation()
        self.assertEqual(self.calls(), [])

    def test_symlink_installation_directory_is_rejected(self):
        outside = self.directory / "outside"
        outside.mkdir()
        self.prefix.mkdir()
        (self.prefix / "bin").symlink_to(outside, target_is_directory=True)
        self.invoke(str(self.archive), successful=False)
        self.assertEqual(list(outside.iterdir()), [])

    def test_replacement_failure_rolls_back_binaries_and_metadata(self):
        self.seed_installation()
        extracted = self.directory / "extracted"
        extracted.mkdir()
        installer.extract_archive(self.archive, extracted)
        replace = os.replace

        def fail_metadata(source, target):
            if Path(source).name == "release" and Path(target) == self.prefix / "share/wm/release":
                raise OSError("fixture replacement failure")
            replace(source, target)

        with patch.object(installer.os, "replace", side_effect=fail_metadata):
            with self.assertRaisesRegex(OSError, "fixture replacement failure"):
                installer.install(extracted, self.prefix)
        self.assert_old_installation()

    def test_failed_rollback_retains_original_binaries_for_recovery(self):
        self.seed_installation()
        extracted = self.directory / "extracted"
        extracted.mkdir()
        installer.extract_archive(self.archive, extracted)
        replace = os.replace

        def fail_install_and_restore(source, target):
            if Path(source).name in {"release", "backup-0"}:
                raise OSError("fixture replacement failure")
            replace(source, target)

        with patch.object(installer.os, "replace", side_effect=fail_install_and_restore):
            with self.assertRaisesRegex(installer.InstallError, "backups remain"):
                installer.install(extracted, self.prefix)
        backups = list(self.prefix.glob(".wm-install-*/backup-0"))
        self.assertEqual(len(backups), 1)
        self.assertEqual(backups[0].read_bytes(), b"old-daemon")

    @unittest.skipUnless(sys.platform == "darwin", "requires macOS codesign")
    def test_real_codesign_rejects_a_binary_with_another_signer(self):
        extracted = self.directory / "extracted"
        extracted.mkdir()
        installer.extract_archive(self.archive, extracted)
        shutil.copyfile("/bin/echo", extracted / "wm")
        with self.assertRaises(installer.InstallError):
            installer.verify_release(extracted)


if __name__ == "__main__":
    unittest.main()
