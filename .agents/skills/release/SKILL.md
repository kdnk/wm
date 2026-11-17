---
name: release
description: Prepare or publish a wm release through the existing GitHub Actions workflow, choosing the semantic version from unreleased changes when the user omits it. Use for release requests, version bumps, and release retries.
---

# Release wm

Choose the version, prepare the release change, and use the existing release workflow when publication is authorized.

## Establish the release scope

Read `AGENTS.md`, `docs/release-signing.md`, `.github/workflows/release.yml`, `scripts/verify-local.sh`, and `scripts/release.py`. Treat these files as the current release contract.

Inspect `but status -fv` and `but diff`. Preserve unrelated work, including an existing version bump. Work in the existing checkout on a dedicated GitButler branch. Create dependent branches with `but branch new NAME --above BASE`.

Use `gh` for remote GitHub operations. Inspect published releases, tags, the remote `main` SHA, and existing release runs:

```sh
gh release list --repo kdnk/wm --limit 30
gh api repos/kdnk/wm/commits/main --jq .sha
gh run list --repo kdnk/wm --workflow release.yml --limit 10
```

Determine the intended source commit and the last applicable published release on its history. Compare the actual diff and commit history since that release. Fetch missing history with `but pull --check` when needed. Account for intended uncommitted changes, but exclude unrelated applied branches. Do not release a GitButler workspace merge commit.

## Choose the semantic version

Choose the version yourself unless the user supplies one. Explain the choice with concrete changes. Commit prefixes are hints, not evidence of compatibility.

Assess compatibility across CLI commands, configuration, IPC, installation, and documented window-management behavior. Use the highest applicable category:

- At or above `1.0.0`, increment major for incompatible changes, minor for compatible features, and patch for compatible fixes.
- Below `1.0.0`, increment minor for new features or incompatible changes. Increment patch for compatible fixes, performance improvements, or release packaging corrections. Describe migrations for incompatible changes.
- Reset lower components when incrementing major or minor. For example, a feature after `0.8.2` becomes `0.9.0`.
- Keep the project below `1.0.0` unless the user requests a stable API milestone. Use prerelease identifiers only when requested. Avoid build metadata for ordinary releases.
- Documentation, tests, and internal maintenance alone do not require a release. If the user explicitly requests a release anyway, choose the smallest valid patch increment.

Inspect root `[package].version` alongside remote tags and releases. Reuse a pending bump if the version fits the complete release scope and is still available. If the bump is too small, explain and update it. Choose a version above the applicable published versions, accounting for any existing pending bump.

An existing tag reserves its version for its exact source commit, even if publication failed. Retry that same commit or choose a higher version for changed source. Never move a tag or replace published assets. If a user-specified version conflicts with compatibility or an existing tag, explain before publication.

## Prepare and verify the release change

Update root `[package].version` in `Cargo.toml` and the matching `wm` entry in `Cargo.lock`. Keep dependency resolution unchanged. The `wm` binary uses the root version. Do not bump `wm-client`, `wm-protocol`, or vendored `cgs` solely to match the binary release.

Review with `but diff` and `but status -fv`. Verify that the manifest and lockfile agree and only intended files changed. When committing is authorized, use an English Conventional Commit message with a body explaining why and what changed. Select only the release files with `but`.

Run `bash scripts/verify-local.sh` locally against the exact candidate before publication. GitHub Actions does not run tests or formatting checks. Run `bash scripts/verify-dot-port.sh` when imported Rift features changed. For changes to release automation, signing setup, or installation, run the corresponding fixture test:

```sh
python3 tests/release-workflow-test.py
python3 tests/release-signing-setup-test.py
python3 tests/install-release-test.py
```

Keep fixture tests isolated from the user's installation, service, signing material, keychains, and certificate trust. Live desktop tests require a matching daemon and macOS permissions. Report checks that could not run.

Summarize the version rationale, release contents, verification results, and exact candidate commit before publication. When publication is authorized below, continue without asking for confirmation of the summary. Preparation-only requests end with the local change or commit.

## Publish through the existing workflow

An explicit `$release` invocation or direct request to release or publish authorizes the necessary version preparation, local commit, merge into local `main`, push of `main`, existing workflow dispatch when needed, confirmation of publication, and local installation of the published version. Carry that authorization through the release without asking again after the candidate summary. Respect narrower preparation-only, commit-only, publication-only, or other explicit limits. Creating or editing this skill, a version-bump request alone, and automatic skill selection do not authorize publication or installation.

For sandbox approval justifications, quote the actual user request and name the exact candidate SHA and intended side effects, including the local `main` merge, remote `main` push, and resulting release workflow. For installation, also cite the standing user authorization below and state the installed path, service interruption, and layout restoration. A routine sandbox escalation does not require another user confirmation. Do not substitute a generic claim that the skill authorizes publication. If tool approval review denies an action, report the denied action and stated reason honestly. Do not bypass the denial or retry unchanged without new authorization or evidence that addresses the denial. Ask only for authorization still required by that denial or an explicit user limit.

Follow the repository's GitButler path through local `main`, without creating a PR or merging through GitHub. Confirm that the configured target is `main`, then use `but merge RELEASE_BRANCH --ff-only --yes` to merge and push the verified candidate. Publication authorization includes this push, so do not apply the local-only remote override used for unpublished landing. If fast-forwarding is impossible, reassess and verify the resulting candidate before publication rather than silently creating a different release commit.

The normal path is a root version change merged into `main` after local verification. That push starts `release.yml`, which uses the pinned source SHA, creates an immutable tag, builds ARM64 only, signs the `wm` binary, and publishes the archive. Keep the legacy universal archive filename for existing installers. Keep tag creation and publication in that workflow. Do not create tags or releases manually with `scripts/release.py tag`, `scripts/release.py publish`, or `gh release create`.

For an authorized first release or retry, dispatch against `main` after confirming the remote root version and source commit:

```sh
gh workflow run release.yml --repo kdnk/wm --ref main
```

Record the run ID for the intended source SHA. Confirm the run's `headSha` before relying on its result. If `main` moved, reassess the scope instead of treating a different commit as verified. Follow the exact run with `gh run view` or `gh run watch`. Do not dispatch another run while a matching run is active.

For failures, inspect `gh run view RUN_ID --repo kdnk/wm --log-failed`. Retry the same source only after identifying the failure. If source changes are needed after tagging, choose a new version. A published release is complete and must remain unchanged. Report persistent failures rather than repeatedly dispatching or modifying release infrastructure.

Preserve the fixed identity pinned in `assets/release-signing.pem`. Ordinary releases use existing Actions secrets. If signing material is missing, follow `docs/release-signing.md` and restore the protected backup. Do not generate a replacement identity or expose secrets.

## Confirm publication

Confirm workflow success, a published release, and a version tag pointing to the intended source SHA. Inspect the release with `gh release view` and the tag with `gh api`. Resolve annotated tags to their commit when necessary.

Check for the nonempty `wm-universal-macos-VERSION.tar.gz` asset. Confirm that the workflow verified the packaged binary version and the pinned certificate.

Report the version and rationale, source commit, checks, and descriptive links to the release and workflow run. For local preparation, report the branch and changed files instead. State remaining failures or required publication actions explicitly.

## Install the published version

The repository owner has given standing authorization for future explicit `$release` requests to replace the existing installed wm binary with the verified published version, briefly stop and restart an already running wm service, and save and restore its layout. The owner explicitly asked not to be asked for this installation permission again. Treat the current invocation together with this standing authorization as approval for these concrete local effects, unless the user narrows the request. This permission does not authorize installing or enabling a new service or changing certificate trust.

After confirming publication, install the exact published version unless the user limited the task to publication. Download `wm-universal-macos-VERSION.tar.gz` from `vVERSION` with `gh release download` into a temporary directory. Run `python3 scripts/install-release.py PATH_TO_ARCHIVE` so the existing installer verifies the pinned certificate and signature before replacing installed files. Preserve the existing installation prefix with `--prefix` when needed. The default prefix is `~/.local`.

Before stopping a running service, save its layout and query its window inventory, order, and sizes. Follow the stop, update, and start sequence in `docs/release-signing.md`. Wait for the original window inventory before loading the saved layout, then verify restored order and sizes with fresh queries. Restore the prior running state and layout if installation fails. Leave a previously stopped service stopped. Do not install or enable a new service as part of the update. Run the installed binary by its absolute path with `--version` and confirm that it matches the published version. Include the installed path and version in the final report. Report installation failures separately from successful publication, without republishing the release.
