#!/usr/bin/env python3
# Modified by kdnk for wm on 2026-11-14. Original attribution is preserved in LICENSE.
"""Create or reuse a self-signed wm release identity and register its GitHub Secrets."""

import argparse
import base64
from contextlib import contextmanager
import datetime
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import secrets
import shutil
import stat
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]
FILES = ("identity.p12", "password", "certificate.pem", "identity.json")
SECRET_NAMES = ("WM_SIGNING_P12", "WM_SIGNING_PASSWORD")
OPENSSL = "/usr/bin/openssl"


class SetupError(Exception):
    pass


def openssl(*args, data=None):
    result = subprocess.run([OPENSSL, *args], input=data, stdout=subprocess.PIPE,
                            stderr=subprocess.PIPE)
    if result.returncode:
        raise SetupError(f"OpenSSL {args[0]} failed; inspect the local signing identity.")
    return result.stdout


def protected(path, directory=False):
    info = path.lstat()
    expected = stat.S_ISDIR if directory else stat.S_ISREG
    if not expected(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077:
        raise SetupError(f"Unsafe signing path or permissions: {path}")


@contextmanager
def identity_lock(path):
    path.parent.mkdir(parents=True, exist_ok=True)
    lock = path.parent / ("." + path.name + ".lock")
    descriptor = os.open(lock, os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600)
    try:
        protected(lock)
        fcntl.flock(descriptor, fcntl.LOCK_EX)
        yield
    finally:
        os.close(descriptor)


def write_metadata(directory, metadata):
    descriptor, temporary = tempfile.mkstemp(prefix=".identity-", dir=directory)
    try:
        with os.fdopen(descriptor, "w") as output:
            json.dump(metadata, output, indent=2, sort_keys=True)
            output.write("\n")
            output.flush()
            os.fsync(output.fileno())
        os.replace(temporary, directory / "identity.json")
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def der_element(data, start):
    offset = start + 1
    length = data[offset]
    offset += 1
    if length & 0x80:
        count = length & 0x7f
        length = int.from_bytes(data[offset:offset + count], "big")
        offset += count
    return offset, offset + length


def verify_self_signature(certificate, der):
    body, _ = der_element(der, 0)
    _, tbs_end = der_element(der, body)
    _, algorithm_end = der_element(der, tbs_end)
    signature_start, signature_end = der_element(der, algorithm_end)
    if der[algorithm_end] != 3 or der[signature_start] != 0:
        raise SetupError("Invalid certificate signature encoding.")
    with tempfile.TemporaryDirectory(prefix="wm-certificate-check-") as temporary:
        directory = Path(temporary)
        key = directory / "public.pem"
        signature = directory / "signature"
        key.write_bytes(openssl("x509", "-in", str(certificate), "-pubkey", "-noout"))
        signature.write_bytes(der[signature_start + 1:signature_end])
        openssl("dgst", "-sha256", "-verify", str(key), "-signature", str(signature),
                data=der[body:tbs_end])


def certificate_info(certificate):
    der = openssl("x509", "-in", str(certificate), "-outform", "DER")
    openssl("x509", "-in", str(certificate), "-checkend", "0", "-noout")
    issuer = openssl("x509", "-in", str(certificate), "-issuer", "-noout").split(b"=", 1)[1].strip()
    subject = openssl("x509", "-in", str(certificate), "-subject", "-noout").split(b"=", 1)[1].strip()
    if issuer != subject:
        raise SetupError("The release signing certificate must be self-issued.")
    verify_self_signature(certificate, der)
    expiry = openssl("x509", "-in", str(certificate), "-enddate", "-noout").decode().strip()
    return {
        "certificate_sha1": hashlib.sha1(der).hexdigest().upper(),
        "certificate_sha256": hashlib.sha256(der).hexdigest().upper(),
        "expires": expiry.removeprefix("notAfter="),
    }


def validate(directory, repo):
    protected(directory, directory=True)
    for name in FILES:
        path = directory / name
        if not path.exists() or path.is_symlink():
            raise SetupError(f"Incomplete signing identity: {directory}")
        protected(path)
    try:
        metadata = json.loads((directory / "identity.json").read_text())
    except (ValueError, UnicodeError) as error:
        raise SetupError("Invalid signing identity metadata.") from error
    if not isinstance(metadata, dict) or metadata.get("version") != 1 or metadata.get("repository") != repo:
        raise SetupError("Signing identity metadata does not match this repository.")
    uploaded = metadata.get("uploaded")
    if not isinstance(uploaded, dict) or set(uploaded) - set(SECRET_NAMES):
        raise SetupError("Invalid signing upload receipts.")
    if any(value is not None and not isinstance(value, str) for value in uploaded.values()):
        raise SetupError("Invalid signing upload receipt timestamp.")
    password = (directory / "password").read_bytes()
    if len(password.strip()) < 32 or password.count(b"\n") > 1:
        raise SetupError("Invalid signing identity password file.")
    info = certificate_info(directory / "certificate.pem")
    if any(metadata.get(key) != value for key, value in info.items()):
        raise SetupError("Signing certificate differs from its recorded identity.")
    arguments = ("pkcs12", "-in", str(directory / "identity.p12"),
                 "-passin", "file:" + str(directory / "password"))
    certificate = openssl(*arguments, "-clcerts", "-nokeys")
    actual_der = openssl("x509", "-outform", "DER", data=certificate)
    expected_der = openssl("x509", "-in", str(directory / "certificate.pem"), "-outform", "DER")
    if actual_der != expected_der:
        raise SetupError("The PKCS#12 certificate differs from the public certificate.")
    private = openssl(*arguments, "-nocerts", "-nodes")
    public = openssl("pkey", "-pubout", "-outform", "DER", data=private)
    certificate_key = openssl("x509", "-in", str(directory / "certificate.pem"), "-pubkey", "-noout")
    if public != openssl("pkey", "-pubin", "-outform", "DER", data=certificate_key):
        raise SetupError("The PKCS#12 key does not match the certificate.")
    return metadata


def prepare(directory, repo):
    if directory.exists() or directory.is_symlink():
        return validate(directory, repo)
    scratch = Path(tempfile.mkdtemp(prefix=".wm-signing-", dir=directory.parent))
    try:
        (scratch / "password").write_text(secrets.token_urlsafe(48) + "\n")
        config = scratch / "openssl.cnf"
        config.write_text("""[req]
prompt = no
distinguished_name = subject
x509_extensions = codesign
[subject]
CN = wm Release Signing
O = kdnk
[codesign]
basicConstraints = critical,CA:false
keyUsage = critical,digitalSignature
extendedKeyUsage = codeSigning
""")
        private = scratch / "private-key.pem"
        certificate = scratch / "certificate.pem"
        openssl("req", "-new", "-newkey", "rsa:3072", "-x509", "-sha256", "-nodes",
                "-days", "3650", "-config", str(config), "-keyout", str(private),
                "-out", str(certificate))
        openssl("pkcs12", "-export", "-inkey", str(private), "-in", str(certificate),
                "-out", str(scratch / "identity.p12"), "-descert", "-keypbe", "PBE-SHA1-3DES",
                "-macalg", "sha256", "-passout", "file:" + str(scratch / "password"))
        private.unlink()
        config.unlink()
        metadata = {"version": 1, "repository": repo,
                    "created_at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
                    "uploaded": {}, **certificate_info(certificate)}
        write_metadata(scratch, metadata)
        validate(scratch, repo)
        os.rename(scratch, directory)
        return metadata
    finally:
        if scratch.exists():
            shutil.rmtree(scratch)


def github(*args, data=None):
    result = subprocess.run(["gh", *args], input=data, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    if result.returncode:
        raise SetupError(f"GitHub {args[0]} {args[1]} failed; existing signing Secrets were not replaced.")
    return result.stdout


def remote_secrets(repo):
    raw = github("secret", "list", "--repo", repo, "--json", "name,updatedAt")
    try:
        rows = json.loads(raw)
        if not isinstance(rows, list):
            raise ValueError()
        return {row["name"]: row.get("updatedAt") for row in rows}
    except (ValueError, TypeError, KeyError) as error:
        raise SetupError("GitHub returned an invalid Secret list.") from error


def register(directory, metadata, repo, remote):
    present = set(remote) & set(SECRET_NAMES)
    if present == set(SECRET_NAMES):
        return "already_configured"
    if present - set(metadata["uploaded"]):
        raise SetupError("Only one signing Secret exists without a matching local upload receipt; leaving it unchanged.")
    for name in SECRET_NAMES:
        remote = remote_secrets(repo)
        if name in remote:
            if name not in metadata["uploaded"] or (
                metadata["uploaded"][name] is not None and metadata["uploaded"][name] != remote[name]
            ):
                raise SetupError(f"Existing {name} differs from the local upload receipt; leaving it unchanged.")
            continue
        if name == "WM_SIGNING_P12":
            payload = base64.b64encode((directory / "identity.p12").read_bytes())
        else:
            payload = (directory / "password").read_bytes().rstrip(b"\n")
        github("secret", "set", name, "--repo", repo, data=payload)
        metadata["uploaded"][name] = None
        write_metadata(directory, metadata)
        remote = remote_secrets(repo)
        if name not in remote:
            raise SetupError(f"GitHub did not report {name} after upload; rerun to inspect the saved receipt.")
        metadata["uploaded"][name] = remote[name]
        write_metadata(directory, metadata)
    return "registered"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", default="kdnk/wm")
    parser.add_argument("--identity-dir", type=Path,
                        default=Path.home() / ".local/share/wm/release-signing")
    parser.add_argument("--local-only", action="store_true")
    parser.add_argument("--pinned-certificate", type=Path, default=ROOT / "assets/release-signing.pem")
    args = parser.parse_args()
    if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", args.repo):
        raise SetupError("Use an owner/repository name for --repo.")
    directory = args.identity_dir.expanduser().absolute()
    resolved = directory.resolve()
    if resolved == ROOT or ROOT in resolved.parents:
        raise SetupError("The signing identity directory must be outside the repository.")
    os.umask(0o077)
    with identity_lock(directory):
        remote = {} if args.local_only else remote_secrets(args.repo)
        present = set(remote) & set(SECRET_NAMES)
        if not directory.exists() and not directory.is_symlink() and present:
            if present == set(SECRET_NAMES):
                print(json.dumps({"status": "already_configured", "repository": args.repo}))
                return
            raise SetupError("Only one signing Secret exists; no local identity can establish its provenance.")
        if args.pinned_certificate.exists() and not directory.exists():
            raise SetupError("A release certificate is pinned; restore its local signing backup instead of generating another identity.")
        metadata = prepare(directory, args.repo)
        if args.pinned_certificate.exists():
            expected = openssl("x509", "-in", str(args.pinned_certificate), "-outform", "DER")
            actual = openssl("x509", "-in", str(directory / "certificate.pem"), "-outform", "DER")
            if expected != actual:
                raise SetupError("The local signing identity differs from the pinned release certificate.")
        status = "prepared" if args.local_only else register(directory, metadata, args.repo, remote)
        print(json.dumps({"status": status, "repository": args.repo,
                          "identity_dir": str(directory),
                          **{key: metadata[key] for key in ("certificate_sha1", "certificate_sha256", "expires")}}))


if __name__ == "__main__":
    try:
        main()
    except (SetupError, OSError) as error:
        raise SystemExit(str(error))
