# Rift fork

- Use `but` for version-control writes. Work in the existing checkout on a dedicated branch; do not create worktrees.
- The `cgs/` directory contains vendored cgs sources with the exact-capture extension required by live resizing. Preserve its license and record source updates in `cgs/README.fork.md`.
- Run `bash scripts/verify-dot-port.sh` after changing the imported Rift features. Pass a configuration file as the optional argument to validate additional app rules.
- The verification script tests LaunchAgent serialization with an isolated HOME. It must not start or stop the user's service, change signing identities, or install the daemon.
- Desktop tests under `tests/rift-*-live-test.py` require a running matching daemon and macOS permissions. Native geometry tests alone do not prove composited content rendering; retain display video and daemon traces for live resize verification.
- Unit tests must isolate mouse-state reads from the real WindowServer. Synthetic frame-change events must specify the mouse state needed by the case.
- The fork's commands are `wm` and `wm-cli`. Keep Rift names in upstream attribution, archived import references, and historical verifier filenames.
- Create dependent branches with `but branch new NAME --above BASE` so their changes can reference commits on the base branch.
- Release signing uses the fixed identity prepared by `scripts/setup-release-signing.py` and pinned in `assets/release-signing.pem`. Restore the protected backup if local signing material is missing; do not generate a replacement for an established identity.
- The fixed self-signed identity has no Team ID. macOS ignores legacy `AssociatedBundleIdentifiers` for this identity. App-bundle packaging alone does not prove a background-item name change; verify the native settings UI and backgroundtaskmanagement logs before changing release or service formats.
- Run `python3 tests/release-signing-setup-test.py` after changing signing setup. Fixtures must not register live Secrets or change keychains or certificate trust.
- Run `python3 tests/release-workflow-test.py` after changing release automation. Keep verification, tag creation, and Release publication connected in one workflow; events generated with `GITHUB_TOKEN` do not start other ordinary workflows.
- Reusable CI invoked by a release must use its pinned `source_sha` for checkout and concurrency so another version's verification cannot cancel it.
- Run `python3 tests/install-release-test.py` after changing release installation. Keep archive/signature failures from replacing installed binaries; fixtures must not alter the real installation, service, signing material, keychains, or certificate trust.
- `wm close-window` presses the native close button; `wm-cli execute window close` retains Command-W behavior. Run the `sys::close_window` and wm-client tests after changing close behavior, and use `tests/close-window-live-test.py` with its PID-and-title-checked fixture for native verification.
