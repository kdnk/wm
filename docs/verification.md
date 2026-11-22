# Verify changes to wm

## Local checks

Use an Apple silicon Mac with Apple's Command Line Tools. Install Rust 1.95.0, stable, and nightly with rustfmt. Run verification locally. GitHub Actions builds, signs, and publishes releases.

| When | Command | Coverage |
| --- | --- | --- |
| Changes to code or release tooling | `bash scripts/verify-local.sh` | Minimum and stable Rust checks, nightly formatting, workspace tests, attribution, and release fixtures. Run it against the exact release candidate before publishing. |
| Changes to imported Rift behavior, configuration parsing, native AppKit integration, the CLI contract, or LaunchAgent generation | `bash scripts/verify-integration.sh` | Run alongside the local checks from a macOS desktop session. The script validates the default configuration, runs native AppKit fixtures, and checks the built CLI and LaunchAgent XML. |
| Rust formatting during iteration | `cargo +nightly fmt --all --check` | Run separately for a quick check. A successful `verify-local.sh` run already covers formatting. |
| Additional app rules | `bash scripts/verify-integration.sh /absolute/path/to/config.toml` | Validate the supplied configuration in place of the default. |
| Signing setup, release automation, or release installation | `python3 tests/release-signing-setup-test.py`, `python3 tests/release-workflow-test.py`, or `python3 tests/install-release-test.py`, respectively | Run the relevant fixture for targeted feedback. The full local check runs all three. |

The integration script writes and parses a LaunchAgent plist in a temporary home directory. It leaves the running service, installed binary, and signing identity alone. For automated AppKit checks, confirm that the output includes the expected `PASS` summaries as well as a zero exit status.

## Desktop checks

### Isolated native fixtures

These checks run in a macOS GUI session and use their own fixture windows. They can run without replacing the daemon.

| Changed behavior | Run | Check |
| --- | --- | --- |
| Menu interaction | `cargo run --locked --example menu-bar-window-list-check -- --interactive` | Check icon order, offscreen click-to-focus, workspace switching, overflow, and the wm controls menu. |
| Menu motion | `cargo build --locked --example menu-bar-window-list-check`, then `python3 tests/menu-motion-live-test.py --output NEW_DIRECTORY` | With Screen Recording access, retain the video of focus and workspace reversals and drag-start settlement. |
| Native window borders | `cargo run --locked --example window-borders-check` | With Screen Recording access, check composited borders and hit testing. For flicker, record the affected window and compare border pixels with WindowServer stacking order during the same interaction. |
| Native close button | `cargo build --locked --example close-window-check`, then `python3 tests/close-window-live-test.py` | Grant Accessibility access. Verify that the native close button closes the disposable two-tab window. The fixture identifies the window by PID and unique title. |

### Checks with a running daemon

Use a daemon built from the code under test, a matching client, Accessibility access, and a disposable scrolling workspace. Grant Screen Recording access for checks that capture pixels or video. Pass the matching binary with `--client` because fixture defaults vary. The Qt fixtures use Anki's bundled Python and PyQt6 by default. Pass `--qt-python` and `--qt-packages` for a different runtime.

Run fixture commands from the repository root. Replace `NEW_DIRECTORY`, `REPORT.json`, and `TRACE` with fresh output locations and the actual trace path as applicable.

| Changed behavior | Run | Check |
| --- | --- | --- |
| Settings-window focus | `python3 tests/rift-settings-focus-live-test.py --app muse` or `--app anki` | Check native focus and tiled geometry through repeated focus changes. |
| Window lifecycle | `python3 tests/rift-window-lifecycle-live-test.py` | Check native close, hide, and minimize against the tiled layout. |
| Resize and retarget motion | `python3 tests/rift-motion-live-test.py --output REPORT.json` and `python3 tests/rift-content-render-live-test.py --output REPORT.json --video-dir NEW_DIRECTORY` | Use distinct report paths. Inspect native geometry and painted content during resizing. Retain screen video and daemon traces for live resize verification. |
| Hover focus during a focus transition | `python3 tests/motion-polish-live-test.py --focus-transition-check --output NEW_DIRECTORY` | Check native key-window focus during motion and hover recovery on fresh movement. |
| Mouse-motion sampling | `python3 tests/hover-focus-live-test.py --output NEW_DIRECTORY` | The final boundary crossing must acquire native focus while the pointer remains still, without a later mouse event. For Screen Zoom, cross in both directions with Control released and stop immediately after crossing. |
| Dialog following | `python3 tests/hover-focus-live-test.py --follow-parent --output NEW_DIRECTORY`, then check both IntelliJ search dialogs manually | Compare the native editor and dialog frames during and after scrolling. IntelliJ can report its tiled editor as an `AXDialog` while a picker is open. |
| Width-command anchoring | `python3 tests/pointer-width-live-test.py --run --output NEW_DIRECTORY --video --trace-file TRACE` | Check both pointer-selected edges and arbitrary widths. Use the matching daemon's stderr log with info-level tracing. |

For picker drags, measure the dialog's offset from its editor after mouse-up. IntelliJ can move the dialog a shorter distance than the pointer. Scroll away and back to check that wm restores the measured offset, including after a drag at a display edge. Check parking against the actual display edge because IntelliJ can clamp a fully offscreen AX position to a visible strip.

For synthetic drags, identify fixture windows by PID and title. Confirm the intended fixture with WindowServer hit testing at the click point before posting mouse-down. A border can overlap the fixture in the window list while ignoring mouse input. Verify that the drag commits a layout change and that the test restores runtime settings, workspace, and cursor position even on failure.

For native title-bar drags between stacked displays, keep `mouse_follows_focus` enabled when checking pointer jumps. Sample the actual WindowServer pointer position while the button is held through the crossing. Check that the tile stays on the source while dragging. Release just past the shared edge while the window center is still on the source display. Try both directions and a drag that crosses the edge and returns before release.

For current native resize animation, use the resize and content checks in the table. The archived `rift-live-resize-live-test.py` and `rift-multi-resize-live-test.py` scripts target the former compositor and require `WM_LIVE_RESIZE_TRACE`, which current wm no longer emits. See the [original import notes](upstream-sync.md#original-import-from-dot) for the historical implementation and comparison procedure.

## Temporarily replace a running daemon

Use this procedure when a desktop check needs a candidate daemon in place of a running installation. Keep the recovery files until the original daemon and layout are verified.

1. Prepare the candidate before stopping the original. Preserve the installed wm's designated signing requirement and [pinned release certificate](../assets/release-signing.pem). If `codesign` rejects the untrusted self-signed identity, use file-based signing without changing certificate trust. Keep any temporary private-key file owner-only and delete it after signing. If macOS denies Accessibility at a temporary path, use the existing authorized install path.
2. Record the original executable path and service plist. Back up the binary and plist in a private directory. With the matching installed client, save the output of `wm query displays` and `wm query workspaces --display UUID` for each affected display. Record the original window IDs and PIDs, workspace assignments, relative order, and sizes. Run `wm execute save-layout /absolute/path/to/original-layout.ron` with a path outside the master layout file.
3. Use the original installed executable for `wm service stop`. Continue only after it succeeds. Wait until `launchctl print "gui/$(id -u)/git.kdnk.wm"` reports that the job is absent. Back up the checkpoint at `~/.wm/layout.ron` before starting the candidate.
4. Put the candidate at the authorized path if required. Start it and run the desktop check. For a service-managed candidate, point the standard LaunchAgent plist at the candidate and use the enable and bootstrap commands below. Keep the backed-up checkpoint separate because stopping a candidate that shares the original master layout path can overwrite the master file.
5. Restore the original installation even if the test fails. Stop a service-managed candidate using its own matching executable, and wait for the job to disappear again. If the candidate was launched separately, stop that process and wait for it to exit. Restore the backed-up binary, plist, and checkpoint before restarting the original job. For the default plist path, run:

   ```sh
   launchctl enable "gui/$(id -u)/git.kdnk.wm"
   launchctl bootstrap "gui/$(id -u)" "$HOME/Library/LaunchAgents/git.kdnk.wm.plist"
   ```

   Use the backed-up plist's original path if it differs. If a custom plist does not start the job at bootstrap, run `launchctl kickstart "gui/$(id -u)/git.kdnk.wm"`.
6. Return each display to its original macOS Space if the test switched Spaces. Run `wm query workspaces --display UUID` with the restored client until every original window appears. Allow additional windows. Then run `wm execute load-layout /absolute/path/to/original-layout.ron --scope displays`. The scope covers the active macOS Space on each display, including its virtual workspaces. After the layout settles, query again. Match the original windows by ID and PID and verify their workspace assignments, relative order, and sizes. Filter additional windows out of the comparison. The fresh queries confirm whether restoration succeeded.
