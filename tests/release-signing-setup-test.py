#!/usr/bin/env python3
# Modified by kdnk for wm on 2026-11-14. Original attribution is preserved in LICENSE.

import base64
import hashlib
import json
import os
from pathlib import Path
import shutil
import stat
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts/setup-release-signing.py"
OPENSSL = "/usr/bin/openssl"
FAKE_GH = r'''#!/usr/bin/env python3
import hashlib,json,os,sys
from pathlib import Path
path=Path(os.environ['GH_TEST_STATE'])
state=json.loads(path.read_text())
args=sys.argv[1:]
state['calls'].append(args)
if args[:2] == ['secret','list']:
    path.write_text(json.dumps(state))
    if os.environ.get('GH_TEST_LIST_ERROR'):
        raise SystemExit(7)
    print(json.dumps([{'name':name,'updatedAt':value['updatedAt']} for name,value in state['secrets'].items()]))
elif args[:2] == ['secret','set']:
    name=args[2]
    if os.environ.get('GH_TEST_FAIL_SECRET') == name:
        path.write_text(json.dumps(state))
        raise SystemExit(8)
    payload=sys.stdin.buffer.read()
    state['secrets'][name]={'sha256':hashlib.sha256(payload).hexdigest(),'length':len(payload),
                           'updatedAt':'upload-'+str(len(state['calls']))}
    path.write_text(json.dumps(state))
else:
    raise SystemExit(9)
'''


class SigningSetupTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.baseline_temp = tempfile.TemporaryDirectory(prefix="wm-signing-baseline-")
        cls.baseline = Path(cls.baseline_temp.name) / "identity"
        result = subprocess.run([sys.executable, str(SCRIPT), "--identity-dir", str(cls.baseline),
                                 "--local-only", "--pinned-certificate", str(Path(cls.baseline_temp.name) / "no-pin")],
                                text=True, capture_output=True)
        if result.returncode:
            raise AssertionError(result.stderr)

    @classmethod
    def tearDownClass(cls):
        cls.baseline_temp.cleanup()

    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="wm-signing-fixture-")
        self.addCleanup(self.temporary.cleanup)
        self.directory = Path(self.temporary.name)
        self.identity = self.directory / "identity"
        self.pin = self.directory / "release-signing.pem"
        self.state_path = self.directory / "gh-state.json"
        self.state_path.write_text(json.dumps({"calls": [], "secrets": {}}))
        binary = self.directory / "bin"
        binary.mkdir()
        (binary / "gh").write_text(FAKE_GH)
        (binary / "gh").chmod(0o700)
        self.environment = dict(os.environ, PATH=str(binary) + os.pathsep + os.environ.get("PATH", ""),
                                GH_TEST_STATE=str(self.state_path))

    def seed(self):
        shutil.copytree(self.baseline, self.identity)

    def invoke(self, *extra, successful=True, environment=None, identity=None):
        result = subprocess.run([sys.executable, str(SCRIPT), "--identity-dir", str(identity or self.identity),
                                 "--pinned-certificate", str(self.pin),
                                 *extra], text=True, capture_output=True,
                                env=environment or self.environment)
        if successful:
            self.assertEqual(result.returncode, 0, result.stderr)
        else:
            self.assertNotEqual(result.returncode, 0, result.stdout)
        for secret_file in [self.identity / "password", self.identity / "identity.p12"]:
            if secret_file.is_file():
                secret = secret_file.read_bytes()
                encoded = base64.b64encode(secret).decode() if secret_file.suffix == ".p12" else secret.decode().strip()
                self.assertNotIn(encoded, result.stdout + result.stderr)
        self.assertNotIn("BEGIN PRIVATE KEY", result.stdout + result.stderr)
        self.assertNotIn("BEGIN RSA PRIVATE KEY", result.stdout + result.stderr)
        return result

    def state(self):
        return json.loads(self.state_path.read_text())

    def replace_state(self, state):
        self.state_path.write_text(json.dumps(state))

    def uploads(self):
        return [call[2] for call in self.state()["calls"] if call[:2] == ["secret", "set"]]

    def hashes(self):
        return {path.name: hashlib.sha256(path.read_bytes()).hexdigest() for path in self.identity.iterdir()}

    def test_local_preparation_is_private_real_and_stable(self):
        first = json.loads(self.invoke("--local-only").stdout)
        saved = self.hashes()
        second = json.loads(self.invoke("--local-only").stdout)
        self.assertEqual(first["certificate_sha256"], second["certificate_sha256"])
        self.assertEqual(saved, self.hashes())
        self.assertEqual(stat.S_IMODE(self.identity.stat().st_mode), 0o700)
        self.assertEqual({path.name for path in self.identity.iterdir()},
                         {"identity.p12", "password", "certificate.pem", "identity.json"})
        for path in self.identity.iterdir():
            self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o600)
        inspected = subprocess.check_output([OPENSSL, "x509", "-in", str(self.identity / "certificate.pem"),
                                             "-text", "-noout"], text=True)
        self.assertIn("wm Release Signing", inspected)
        self.assertIn("3072", inspected)
        self.assertIn("CA:FALSE", inspected)
        self.assertIn("Code Signing", inspected)
        self.assertEqual(self.state()["calls"], [])

    def test_uploads_expected_material_and_never_replaces_existing_secrets(self):
        self.seed()
        result = json.loads(self.invoke().stdout)
        self.assertEqual(result["status"], "registered")
        state = self.state()
        p12 = base64.b64encode((self.identity / "identity.p12").read_bytes())
        password = (self.identity / "password").read_bytes().rstrip(b"\n")
        for name, payload in [("WM_SIGNING_P12", p12), ("WM_SIGNING_PASSWORD", password)]:
            self.assertEqual(state["secrets"][name]["sha256"], hashlib.sha256(payload).hexdigest())
        self.assertEqual(json.loads(self.invoke().stdout)["status"], "already_configured")
        self.assertEqual(self.uploads(), ["WM_SIGNING_P12", "WM_SIGNING_PASSWORD"])

    def test_existing_pair_without_local_keys_is_left_untouched(self):
        state = self.state()
        state["secrets"] = {name: {"updatedAt": "external"} for name in ["WM_SIGNING_P12", "WM_SIGNING_PASSWORD"]}
        self.replace_state(state)
        self.assertEqual(json.loads(self.invoke().stdout)["status"], "already_configured")
        self.assertFalse(self.identity.exists())
        self.assertEqual(self.uploads(), [])

    def test_unknown_partial_remote_state_fails_without_creating_keys(self):
        state = self.state()
        state["secrets"] = {"WM_SIGNING_P12": {"updatedAt": "external"}}
        self.replace_state(state)
        self.invoke(successful=False)
        self.assertFalse(self.identity.exists())
        self.assertEqual(self.uploads(), [])
        self.seed()
        self.invoke(successful=False)
        self.assertEqual(self.uploads(), [])

    def test_partial_upload_resumes_only_the_missing_secret(self):
        self.seed()
        environment = dict(self.environment, GH_TEST_FAIL_SECRET="WM_SIGNING_PASSWORD")
        self.invoke(successful=False, environment=environment)
        first_digest = self.state()["secrets"]["WM_SIGNING_P12"]["sha256"]
        self.assertEqual(set(json.loads((self.identity / "identity.json").read_text())["uploaded"]),
                         {"WM_SIGNING_P12"})
        self.invoke()
        self.assertEqual(self.state()["secrets"]["WM_SIGNING_P12"]["sha256"], first_digest)
        self.assertEqual(self.uploads(), ["WM_SIGNING_P12", "WM_SIGNING_PASSWORD", "WM_SIGNING_PASSWORD"])

    def test_changed_remote_receipt_stops_partial_retry(self):
        self.seed()
        self.invoke(successful=False, environment=dict(self.environment, GH_TEST_FAIL_SECRET="WM_SIGNING_PASSWORD"))
        state = self.state()
        state["secrets"]["WM_SIGNING_P12"]["updatedAt"] = "changed-elsewhere"
        self.replace_state(state)
        self.invoke(successful=False)
        self.assertEqual(self.uploads(), ["WM_SIGNING_P12", "WM_SIGNING_PASSWORD"])

    def test_remote_list_failure_is_not_treated_as_empty(self):
        self.invoke(successful=False, environment=dict(self.environment, GH_TEST_LIST_ERROR="1"))
        self.assertFalse(self.identity.exists())
        self.assertEqual(self.uploads(), [])

    def test_invalid_partial_corrupt_permissions_and_symlinks_are_not_replaced(self):
        self.identity.mkdir(mode=0o700)
        self.invoke("--local-only", successful=False)
        self.assertEqual(list(self.identity.iterdir()), [])
        self.identity.rmdir()
        self.seed()
        (self.identity / "identity.p12").write_bytes(b"corrupt")
        saved = self.hashes()
        self.invoke("--local-only", successful=False)
        self.assertEqual(saved, self.hashes())
        shutil.rmtree(self.identity)
        self.seed()
        (self.identity / "password").chmod(0o644)
        self.invoke("--local-only", successful=False)
        (self.identity / "password").chmod(0o600)
        target = self.directory / "saved-password"
        (self.identity / "password").rename(target)
        (self.identity / "password").symlink_to(target)
        self.invoke("--local-only", successful=False)

    def test_certificate_mismatch_and_expiry_are_not_replaced(self):
        self.seed()
        (self.identity / "certificate.pem").write_bytes((self.identity / "identity.p12").read_bytes())
        saved = self.hashes()
        self.invoke("--local-only", successful=False)
        self.assertEqual(saved, self.hashes())
        shutil.rmtree(self.identity)
        self.seed()
        private = subprocess.run([OPENSSL, "pkcs12", "-in", str(self.identity / "identity.p12"),
                                  "-passin", "file:" + str(self.identity / "password"), "-nodes", "-nocerts"],
                                 capture_output=True, check=True).stdout
        key = self.directory / "expired-test-key.pem"
        key.write_bytes(private)
        key.chmod(0o600)
        expired = self.directory / "expired.pem"
        request = self.directory / "request.pem"
        subprocess.run([OPENSSL, "x509", "-x509toreq", "-in", str(self.identity / "certificate.pem"),
                        "-signkey", str(key), "-out", str(request)], capture_output=True, check=True)
        database = self.directory / "database"
        database.write_text("")
        serial = self.directory / "serial"
        serial.write_text("01\n")
        config = self.directory / "ca.cnf"
        config.write_text(f"""[ca]
default_ca = signing
[signing]
database = {database}
serial = {serial}
new_certs_dir = {self.directory}
certificate = {self.identity / 'certificate.pem'}
private_key = {key}
default_md = sha256
policy = policy
[policy]
commonName = supplied
organizationName = optional
""")
        subprocess.run([OPENSSL, "ca", "-selfsign", "-batch", "-config", str(config), "-in", str(request),
                        "-startdate", "20200101000000Z", "-enddate", "20200102000000Z", "-out", str(expired)],
                       capture_output=True, check=True)
        (self.identity / "certificate.pem").write_bytes(expired.read_bytes())
        saved = self.hashes()
        self.invoke("--local-only", successful=False)
        self.assertEqual(saved, self.hashes())

    def test_invalid_metadata_and_self_signature_fail_without_tracebacks(self):
        self.seed()
        (self.identity / "identity.json").write_text("[]")
        result = self.invoke("--local-only", successful=False)
        self.assertNotIn("Traceback", result.stderr)
        shutil.rmtree(self.identity)
        self.seed()
        path = self.identity / "certificate.pem"
        der = bytearray(subprocess.check_output([OPENSSL, "x509", "-in", str(path), "-outform", "DER"]))
        der[-1] ^= 1
        encoded = base64.b64encode(der).decode()
        path.write_text("-----BEGIN CERTIFICATE-----\n" +
                        "\n".join(encoded[index:index + 64] for index in range(0, len(encoded), 64)) +
                        "\n-----END CERTIFICATE-----\n")
        result = self.invoke("--local-only", successful=False)
        self.assertIn("OpenSSL dgst failed", result.stderr)

    def test_public_pin_requires_the_same_identity_and_a_restorable_backup(self):
        self.pin.write_bytes((self.baseline / "certificate.pem").read_bytes())
        self.invoke("--local-only", successful=False)
        self.assertFalse(self.identity.exists())
        self.seed()
        self.invoke("--local-only")
        other = self.directory / "other"
        subprocess.run([sys.executable, str(SCRIPT), "--identity-dir", str(other), "--local-only",
                        "--pinned-certificate", str(self.directory / "other-pin")],
                       capture_output=True, check=True)
        self.pin.write_bytes((other / "certificate.pem").read_bytes())
        self.invoke(successful=False)
        self.assertEqual(self.uploads(), [])

    def test_repository_paths_and_symlink_directories_are_rejected(self):
        self.invoke("--local-only", successful=False, identity=ROOT / ".signing-fixture")
        self.identity.symlink_to(self.baseline, target_is_directory=True)
        self.invoke("--local-only", successful=False)

    def test_concurrent_preparation_reuses_one_identity(self):
        commands = [sys.executable, str(SCRIPT), "--identity-dir", str(self.identity), "--local-only",
                    "--pinned-certificate", str(self.pin)]
        processes = [subprocess.Popen(commands, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                      text=True, env=self.environment) for _ in range(2)]
        results = [process.communicate(timeout=30) for process in processes]
        self.assertEqual([process.returncode for process in processes], [0, 0], results)
        identities = [json.loads(stdout)["certificate_sha256"] for stdout, _ in results]
        self.assertEqual(identities[0], identities[1])


if __name__ == "__main__":
    unittest.main()
