# Set up self-signed releases

wm releases use one persistent, self-signed code-signing identity. GitHub Actions signs both executables with that identity, and the repository pins its public certificate in `assets/release-signing.pem`. The release archive includes the public certificate. Releases are not notarized by Apple.

## Prepare and register the identity

Run from this checkout on a Mac with Python 3, OpenSSL, and GitHub CLI authenticated for the repository:

```sh
python3 scripts/setup-release-signing.py --repo kdnk/wm
```

The command creates a distribution-specific identity under `~/.local/share/wm/release-signing`, then registers `WM_SIGNING_P12` and `WM_SIGNING_PASSWORD` as repository Actions secrets. It does not use the existing local Rift signing identity or change the Mac's keychains or certificate trust.

The local directory has mode `0700`; the encrypted identity and password files have mode `0600`. The directory contains:

| File | Purpose |
| --- | --- |
| `identity.p12` | The encrypted signing certificate and private key used by Actions. |
| `password` | The password protecting the PKCS#12 file. |
| `certificate.pem` | The public certificate, which can be distributed. |
| `identity.json` | The identity fingerprints and registration receipts. |

Keep an encrypted backup of the entire directory. GitHub does not provide a way to download the original secret values. Reuse the identity when moving to another Mac instead of generating a replacement.

Use `--local-only` to create or validate the local identity without accessing GitHub. Use `--identity-dir PATH` to select an existing protected identity directory outside the checkout.

## Repeat setup

The command reuses and validates an existing local identity. It does not silently rotate certificates or overwrite existing signing secrets. If registration stops after one secret succeeds, a local receipt permits a retry to register only the missing secret. An unknown partial remote configuration stops setup for review.

If both signing secrets already exist, setup leaves them unchanged. Check the pinned public certificate before using a signing identity created elsewhere. To change the signing identity, plan an explicit rotation of the backup, both secrets, and the pinned public certificate.

## Publish a release

The release workflow checks that the imported certificate matches the pinned public certificate before trusting it on the temporary Actions runner. It signs `wm` as `git.kdnk.wm` and `wm-cli` as `git.kdnk.wm.cli`, then packages the signed binaries, default configuration, public certificate, and license notices.

The local setup command does not publish a release. To publish a new version, change `[package].version` in `Cargo.toml` and merge that change into `main`. The release workflow detects the version change, runs the reusable Rust checks, creates the matching tag at that exact commit, builds and signs both architectures, then creates a GitHub release with the completed archive.

The workflow does not need a pre-existing Release. Build or signing failures leave the Release unpublished. An unfinished draft can be completed by retrying; a published release is left unchanged. Existing tags must point to the planned source commit and are never moved by automation.

For the first release or a retry of the current version, run the `release` workflow manually against `main`. The workflow reads the version from the selected commit; it does not require entering an existing tag. A push that changes `Cargo.toml` without changing the root package version does not publish a release.

Tag creation and publication belong to the same workflow run. This avoids relying on another workflow being triggered by a tag or release created with `GITHUB_TOKEN`.

Self-signing does not make the developer identity trusted by Apple. Users may need to approve the initial launch and must grant the Accessibility permission used by wm. Keeping the same certificate and identifiers avoids changing the signing identity during ordinary updates; permission behavior still needs to be checked on supported macOS versions.

## Test changes

```sh
python3 tests/release-signing-setup-test.py
python3 tests/release-workflow-test.py
```

The signing fixtures use real OpenSSL operations and a fake GitHub CLI. The release fixtures exercise planning, immutable tag creation, draft recovery, and publication with fake GitHub responses. They do not create real tags or releases, register secrets, change keychains or trust settings, or operate the wm service.
