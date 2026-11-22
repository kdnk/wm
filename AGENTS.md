# Rift fork

- Use `but` for version-control writes. Work in the existing checkout on a dedicated branch; do not create worktrees.
- When asked to land changes, use `but` to merge into local `main`. Do not create a pull request or merge through GitHub. This repository rule overrides skill workflows that require PRs. Push only when explicitly requested.
- Use `env GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=branch.main.remote GIT_CONFIG_VALUE_0= but merge BRANCH` for local-only landing. Plain `but merge` or `but land` pushes when `main` tracks a remote; the process-only override preserves the persistent tracking configuration.
- An explicit request to run `but land` authorizes its normal remote push and takes precedence over the local-only landing default above. Run `but land` without URL rewrites, remote overrides, or other changes that suppress its push. Use local-only landing only when the user explicitly requests it or asks to land changes without naming `but land`.
- The `cgs/` directory contains vendored cgs sources with the exact-capture extension required by live resizing. Preserve its license and record source updates in `cgs/README.fork.md`.
- When first modifying an upstream source file, add the dated fork notice checked by `python3 scripts/check-fork-notices.py` and preserve the original attribution.
- The fork's command is `wm`; use its `query`, `execute`, and `subscribe` subcommands to control the daemon. Keep Rift names in upstream attribution, archived import references, and historical verifier filenames.
- Create dependent branches with `but branch new NAME --above BASE` so their changes can reference commits on the base branch.
- Release signing uses the fixed identity prepared by `scripts/setup-release-signing.py` and pinned in `assets/release-signing.pem`. Restore the protected backup if local signing material is missing; do not generate a replacement for an established identity.
- The fixed self-signed identity has no Team ID. macOS ignores legacy `AssociatedBundleIdentifiers` for this identity. App-bundle packaging alone does not prove a background-item name change; verify the native settings UI and backgroundtaskmanagement logs before changing release or service formats.
- Keep tag creation and Release publication connected in one workflow. Events generated with `GITHUB_TOKEN` do not start other ordinary workflows.
- Before reporting a release complete, verify the exact candidate locally and check the release workflow for the same source SHA. Report or resolve any failed, cancelled, or pending run.
- Release binaries target Apple silicon only. Preserve the legacy `wm-universal-macos-VERSION.tar.gz` asset name for existing installers.
- Keep archive and signature failures from replacing installed binaries.
- Stop a running service with its matching installed `wm` before replacing the binary. The CLI and daemon must agree on the checkpoint request that saves the layout before shutdown.
- `wm close-window` presses the native close button. `wm execute window close` retains Command-W behavior.

## Verification

- Use the [local checks](docs/verification.md#local-checks) to choose commands for your change. `bash scripts/verify-local.sh` covers tests, the minimum Rust version, and nightly formatting. Run `bash scripts/verify-integration.sh` as well when the guide calls for an integration check. Do not add GitHub Actions test jobs for pushes, pull requests, or releases.
- Run the `sys::close_window` and wm-client tests after changing close behavior, then use the native close fixture in the guide.
- After changing signing setup, release automation, or release installation, run the corresponding fixture in the local checks. Fixtures must leave the user's installation, service, signing material, keychains, certificate trust, and live Secrets untouched.
- Unit tests must isolate mouse-state reads from the real WindowServer. Synthetic frame-change events must specify the mouse state needed by the case.
- Configuration watcher tests must use temporary paths and observe actual filesystem notifications and `ConfigUpdated` events. Do not use the user's configuration or start a daemon for these tests.
- If a configuration watcher test times out waiting for `ConfigUpdated` in a sandbox, rerun the same test with access to macOS filesystem notifications before diagnosing a regression.
- Use the [desktop checks](docs/verification.md#desktop-checks) after changing native menus, borders, close behavior, focus and hover, dialog following, drags, or resize and width behavior. Follow the listed commands, permissions, and pass criteria for the changed behavior.
- When temporarily replacing a running daemon for a live test, follow the [replacement and restoration procedure](docs/verification.md#temporarily-replace-a-running-daemon).
